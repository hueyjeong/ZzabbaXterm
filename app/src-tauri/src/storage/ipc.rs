use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{
    Manager,
    ipc::{CommandArg, CommandItem, Invoke},
};
use tokio::sync::watch;
use uuid::Uuid;

use crate::editor::storage_files::{DownloadTarget, open_upload};
use crate::{
    connections::{profile::MethodRef, with_saved_method},
    ssm::{AwsCli, CancellationToken, CliError, Scope},
};

use super::{
    AwsS3, BucketRow, Downloaded, Error, Failure, ObjectRow, Page, Stage, VerifiedSsoSelection, check_bucket,
};

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Document {
    owner: String,
    generation: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Session {
    session_id: String,
    profile: String,
    region: String,
    endpoint: String,
}

struct Tab {
    document: Document,
    client: Arc<AwsS3>,
    closed: Arc<watch::Sender<bool>>,
    selection: Option<Selection>,
    binding: Option<LoginBinding>,
    expired: bool,
    listing: bool,
    verified_bucket: Option<(String, String)>,
}

#[derive(Clone)]
struct LoginBinding {
    target_id: String,
    selection: Arc<VerifiedSsoSelection>,
}

struct Opening {
    document: Document,
    cancel: Arc<CancellationToken>,
    expired: Option<LoginBinding>,
}

struct Selection {
    id: String,
    target: Target,
    direction: Direction,
    file: Option<ApprovedFile>,
}

enum ApprovedFile {
    Upload(std::fs::File),
    Download(DownloadTarget),
}

impl ApprovedFile {
    fn open(path: &std::path::Path, direction: Direction) -> std::io::Result<Self> {
        match direction {
            Direction::Upload => open_upload(path).map(Self::Upload),
            Direction::Download => DownloadTarget::open(path).map(Self::Download),
        }
    }
}

#[derive(Default)]
struct Registry {
    documents: HashMap<String, u64>,
    openings: HashMap<Uuid, Opening>,
    tabs: HashMap<String, Tab>,
    shutdown: bool,
}

impl Registry {
    fn active(&self, document: &Document) -> bool {
        !self.shutdown
            && self.documents.get(&document.owner).copied().unwrap_or(0) == document.generation
    }

    fn tab(&mut self, document: &Document, id: &str) -> Option<&mut Tab> {
        self.active(document).then_some(())?;
        self.tabs
            .get_mut(id)
            .filter(|tab| tab.document == *document)
    }
}

#[derive(Default)]
pub(crate) struct StorageIpc(Mutex<Registry>);

impl StorageIpc {
    pub(crate) fn document(&self, owner: &str) -> Document {
        let registry = self.0.lock().unwrap();
        Document {
            owner: owner.into(),
            generation: registry.documents.get(owner).copied().unwrap_or(0),
        }
    }

    fn reserve_open(&self, document: &Document, id: Uuid) -> Result<Arc<CancellationToken>, Error> {
        let mut registry = self.0.lock().unwrap();
        if !registry.active(document) || registry.tabs.contains_key(&id.to_string()) || registry.openings.contains_key(&id) {
            return Err(unavailable(Stage::Profile));
        }
        let cancel = Arc::new(CancellationToken::new());
        registry.openings.insert(id, Opening { document: document.clone(), cancel: cancel.clone(), expired: None });
        Ok(cancel)
    }

    async fn open_with<F: Future<Output = Result<AwsS3, Error>>>(
        &self, document: Document, request: OpenRequest, directory: PathBuf, cancel: Arc<CancellationToken>,
        verify: impl FnOnce(String, crate::ssm::DirectSsoConfig) -> F,
    ) -> Result<Session, Error> {
        let OpenRequest { id, target_id } = request;
        let check_id = target_id.clone();
        let operation = async {
            let selection = tauri::async_runtime::spawn_blocking(move || verified_selection(&directory, &check_id))
                .await.map_err(|_| unavailable(Stage::Profile))??;
            let result = verify(selection.scope.region().to_owned(), selection.config.clone()).await;
            Ok::<_, Error>((selection, result))
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(unavailable(Stage::Profile)),
            result = tokio::time::timeout(Duration::from_secs(25), operation) =>
                result.unwrap_or_else(|_| Err(Error::at(Stage::Identity, Failure::TimedOut))),
        };
        let mut registry = self.0.lock().unwrap();
        // 취소 뒤 같은 ID가 다시 예약돼도 이전 작업은 새 예약을 회수할 수 없다.
        if !registry.active(&document) || !registry.openings.get(&id).is_some_and(|entry| Arc::ptr_eq(&entry.cancel, &cancel)) {
            return Err(unavailable(Stage::Profile));
        }
        let opening = registry.openings.remove(&id).ok_or_else(|| unavailable(Stage::Profile))?;
        let (selection, result) = result?;
        let binding = LoginBinding { target_id, selection: Arc::new(selection) };
        match result {
            Ok(client) => {
                let profile = binding.selection.scope.profile().into();
                let region = binding.selection.scope.region().into();
                Ok(Self::insert_tab(&mut registry, id.to_string(), document, profile, region, client, Some(binding)))
            }
            Err(error) => {
                if error.failure == Failure::SsoLoginRequired {
                    registry.openings.insert(id, Opening { expired: Some(binding), ..opening });
                }
                Err(error)
            }
        }
    }

    #[cfg(test)]
    fn insert(
        &self,
        document: Document,
        profile: String,
        region: String,
        client: AwsS3,
    ) -> Option<Session> {
        let mut registry = self.0.lock().unwrap();
        if !registry.active(&document) {
            return None;
        }
        Some(Self::insert_tab(&mut registry, Uuid::new_v4().to_string(), document, profile, region, client, None))
    }

    fn insert_tab(
        registry: &mut Registry, session_id: String, document: Document, profile: String,
        region: String, client: AwsS3, binding: Option<LoginBinding>,
    ) -> Session {
        #[cfg(test)]
        let verified_bucket = binding.is_none().then(|| ("selected-bucket".into(), region.clone()));
        #[cfg(not(test))]
        let verified_bucket = None;
        let endpoint = client.endpoint().into();
        registry.tabs.insert(
            session_id.clone(),
            Tab {
                document,
                client: Arc::new(client),
                closed: Arc::new(watch::Sender::new(false)),
                selection: None,
                binding,
                expired: false,
                listing: false,
                verified_bucket,
            },
        );
        Session {
            session_id,
            profile,
            region,
            endpoint,
        }
    }

    fn close(&self, document: &Document, session_id: &str) -> bool {
        let (opening, tab) = {
            let mut registry = self.0.lock().unwrap();
            let opening = Uuid::parse_str(session_id).ok().and_then(|id| {
                if registry.openings.get(&id)?.document != *document { return None; }
                registry.openings.remove(&id)
            });
            let tab = if opening.is_none() && registry.tab(document, session_id).is_some() {
                registry.tabs.remove(session_id)
            } else {
                None
            };
            (opening, tab)
        };
        if let Some(opening) = opening {
            opening.cancel.cancel();
            return true;
        }
        let Some(tab) = tab else { return false; };
        tab.closed.send_replace(true);
        true
    }

    pub(crate) fn navigate(&self, owner: &str) {
        let (openings, tabs) = {
            let mut registry = self.0.lock().unwrap();
            *registry.documents.entry(owner.into()).or_default() += 1;
            let openings = registry.openings.extract_if(|_, opening| opening.document.owner == owner).map(|(_, opening)| opening).collect::<Vec<_>>();
            let tabs = registry.tabs.extract_if(|_, tab| tab.document.owner == owner).map(|(_, tab)| tab).collect::<Vec<_>>();
            (openings, tabs)
        };
        for opening in openings { opening.cancel.cancel(); }
        for tab in tabs { tab.closed.send_replace(true); }
    }

    pub(crate) fn shutdown(&self) {
        let (openings, tabs) = {
            let mut registry = self.0.lock().unwrap();
            registry.shutdown = true;
            (std::mem::take(&mut registry.openings), std::mem::take(&mut registry.tabs))
        };
        for (_, opening) in openings { opening.cancel.cancel(); }
        for (_, tab) in tabs {
            tab.closed.send_replace(true);
        }
    }

    fn begin_selection(
        &self,
        document: &Document,
        target: Target,
        direction: Direction,
    ) -> Option<String> {
        if target.key.is_empty()
            || target.key.len() > 1024
            || check_bucket(&target.bucket, Stage::ListObjects).is_err()
        {
            return None;
        }
        let mut registry = self.0.lock().unwrap();
        let tab = registry.tab(document, &target.session_id)?;
        if tab.selection.is_some() || !bucket_verified(tab, &target) {
            return None;
        }
        let id = Uuid::new_v4().to_string();
        tab.selection = Some(Selection {
            id: id.clone(),
            target,
            direction,
            file: None,
        });
        Some(id)
    }

    fn finish_selection(
        &self,
        document: &Document,
        session_id: &str,
        id: String,
        selected: Option<Result<ApprovedFile, std::io::Error>>,
    ) -> LocalFileApproval {
        let mut registry = self.0.lock().unwrap();
        let Some(tab) = registry.tab(document, session_id) else {
            return LocalFileApproval::Denied;
        };
        let Some(selection) = tab
            .selection
            .as_mut()
            .filter(|selection| selection.id == id)
        else {
            return LocalFileApproval::Denied;
        };
        match selected {
            Some(Ok(file)) => {
                selection.file = Some(file);
                LocalFileApproval::Approved { approval_id: id }
            }
            Some(_) => {
                tab.selection = None;
                LocalFileApproval::Denied
            }
            None => {
                tab.selection = None;
                LocalFileApproval::Cancelled
            }
        }
    }

    fn cancel_approval(&self, document: &Document, request: &ApprovalId) -> bool {
        let mut registry = self.0.lock().unwrap();
        let Some(tab) = registry.tab(document, &request.session_id) else {
            return false;
        };
        if !tab.selection.as_ref().is_some_and(|selection| {
            selection.id == request.approval_id && selection.file.is_some()
        }) {
            return false;
        }
        tab.selection = None;
        true
    }

    fn consume(
        &self,
        document: &Document,
        request: ApprovedTransfer,
        direction: Direction,
    ) -> Option<Transfer> {
        let mut registry = self.0.lock().unwrap();
        let tab = registry.tab(document, &request.target.session_id)?;
        if !bucket_verified(tab, &request.target) { return None; }
        let selection = tab.selection.as_ref()?;
        if selection.id != request.approval_id
            || selection.direction != direction
            || selection.target != request.target
            || selection.file.is_none()
        {
            return None;
        }
        let selection = tab.selection.take()?;
        Some(Transfer {
            client: tab.client.clone(),
            closed: tab.closed.clone(),
            target: selection.target,
            file: selection.file?,
        })
    }

    async fn list_buckets(
        &self,
        document: &Document,
        request: BucketPageRequest,
    ) -> Result<Page<BucketRow>, Error> {
        let (client, closed) = self.client(document, &request.session_id, Stage::ListBuckets)?;
        let result = until_closed(&closed, Stage::ListBuckets, client.list_buckets(request.token.as_deref())).await;
        self.record_expiration(document, &request.session_id, &result);
        result
    }

    async fn list_objects(
        &self,
        document: &Document,
        request: ObjectPageRequest,
    ) -> Result<Page<ObjectRow>, Error> {
        let stage = Stage::ListObjects;
        let (client, closed) = {
            let mut registry = self.0.lock().unwrap();
            let tab = registry.tab(document, &request.session_id).ok_or_else(|| unavailable(stage))?;
            if tab.listing || tab.expired { return Err(unavailable(stage)); }
            if request.token.is_none() {
                tab.verified_bucket = None;
                tab.selection = None;
            } else if !tab.verified_bucket.as_ref().is_some_and(|(bucket, region)| *bucket == request.bucket && *region == request.region) {
                return Err(unavailable(stage));
            }
            tab.listing = true;
            (tab.client.clone(), tab.closed.subscribe())
        };
        let result = until_closed(&closed, stage, async {
            let regional = client.in_region(&request.region, Stage::HeadBucket)?;
            if request.token.is_none() { regional.check_bucket_region(&request.bucket).await?; }
            regional.list_objects(&request.bucket, &request.prefix, request.token.as_deref()).await
        }).await;
        let mut registry = self.0.lock().unwrap();
        let tab = registry.tab(document, &request.session_id).ok_or_else(|| unavailable(stage))?;
        tab.listing = false;
        mark_expired(tab, &result);
        if result.is_ok() && request.token.is_none() && !tab.expired {
            tab.verified_bucket = Some((request.bucket, request.region));
        }
        result
    }

    async fn count_previous_files(
        &self,
        document: &Document,
        request: PreviousFilesRequest,
    ) -> Result<usize, Error> {
        let stage = Stage::ListObjects;
        let PreviousFilesRequest { session_id, bucket, region, prefix } = request;
        let target = Target { session_id, bucket, region, key: prefix };
        let (client, closed) = {
            let mut registry = self.0.lock().unwrap();
            let tab = registry.tab(document, &target.session_id).ok_or_else(|| unavailable(stage))?;
            if !bucket_verified(tab, &target) { return Err(unavailable(stage)); }
            (tab.client.clone(), tab.closed.subscribe())
        };
        let result = until_closed(&closed, stage, async {
            client.in_region(&target.region, stage)?
                .count_previous_files(&target.bucket, &target.key).await
        }).await;
        let mut registry = self.0.lock().unwrap();
        let tab = registry.tab(document, &target.session_id).ok_or_else(|| unavailable(stage))?;
        if !closed.same_channel(&tab.closed.subscribe()) || !Arc::ptr_eq(&tab.client, &client)
            || !bucket_verified(tab, &target)
        {
            return Err(unavailable(stage));
        }
        mark_expired(tab, &result);
        result
    }

    fn record_expiration<T>(&self, document: &Document, id: &str, result: &Result<T, Error>) {
        if is_login_required(result) && let Some(tab) = self.0.lock().unwrap().tab(document, id) {
            mark_expired(tab, result);
        }
    }

    async fn relogin(&self, document: &Document, id: Uuid, directory: PathBuf) -> Result<(), Error> {
        self.relogin_with(document, id, directory, |current, cancel| async move {
            let cli = AwsCli::probe(&cancel).await?;
            cli.relogin_saved(&current.scope, &current.config, &cancel).await
        }).await
    }

    async fn relogin_with<F: Future<Output = Result<(), CliError>>>(
        &self, document: &Document, id: Uuid, directory: PathBuf,
        login: impl FnOnce(VerifiedSsoSelection, CancellationToken) -> F,
    ) -> Result<(), Error> {
        let (binding, cancel, closed) = {
            let mut registry = self.0.lock().unwrap();
            if !registry.active(document) { return Err(unavailable(Stage::Profile)); }
            if let Some(opening) = registry.openings.get_mut(&id).filter(|opening| opening.document == *document) {
                let binding = opening.expired.take().ok_or_else(|| unavailable(Stage::Profile))?;
                (binding, opening.cancel.clone(), None)
            } else {
                let tab = registry.tab(document, &id.to_string()).ok_or_else(|| unavailable(Stage::Profile))?;
                let binding = tab.binding.as_ref().filter(|_| tab.expired && !tab.listing).ok_or_else(|| unavailable(Stage::Profile))?.clone();
                tab.expired = false;
                tab.verified_bucket = None;
                (binding, Arc::new(CancellationToken::new()), Some(tab.closed.subscribe()))
            }
        };
        let operation = async {
            let current = tauri::async_runtime::spawn_blocking(move || verified_selection(&directory, &binding.target_id))
                .await.map_err(|_| unavailable(Stage::Profile))??;
            if current != *binding.selection { return Err(Error::at(Stage::Profile, Failure::AccountMismatch)); }
            login(current, (*cancel).clone()).await.map_err(|error| Error::at(Stage::Profile, match error {
                CliError::CallerChanged => Failure::AccountMismatch,
                CliError::TimedOut => Failure::TimedOut,
                _ => Failure::LoginFailed,
            }))
        };
        if let Some(closed) = closed {
            let _cancel_on_drop = (*cancel).clone().drop_guard();
            until_closed(&closed, Stage::Profile, operation).await
        } else {
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(unavailable(Stage::Profile)),
                result = operation => result,
            };
            let mut registry = self.0.lock().unwrap();
            if !registry.active(document) || !registry.openings.get(&id).is_some_and(|entry| Arc::ptr_eq(&entry.cancel, &cancel)) {
                return Err(unavailable(Stage::Profile));
            }
            registry.openings.remove(&id);
            result
        }
    }

    fn client(
        &self,
        document: &Document,
        session_id: &str,
        stage: Stage,
    ) -> Result<(Arc<AwsS3>, watch::Receiver<bool>), Error> {
        let mut registry = self.0.lock().unwrap();
        let tab = registry
            .tab(document, session_id)
            .ok_or_else(|| unavailable(stage))?;
        Ok((tab.client.clone(), tab.closed.subscribe()))
    }

    async fn upload(&self, document: &Document, request: ApprovedTransfer) -> Result<u64, Error> {
        let transfer = self
            .consume(document, request, Direction::Upload)
            .ok_or_else(|| unavailable(Stage::UploadSource))?;
        let ApprovedFile::Upload(file) = transfer.file else { return Err(unavailable(Stage::UploadSource)); };
        let target = transfer.target;
        let regional = transfer.client.in_region(&target.region, Stage::PutObject)?;
        let result = until_closed(&transfer.closed.subscribe(), Stage::PutObject, regional.upload(&target.bucket, &target.key, file)).await;
        self.record_expiration(document, &target.session_id, &result);
        result
    }

    async fn download(
        &self,
        document: &Document,
        request: ApprovedTransfer,
    ) -> Result<Downloaded, Error> {
        let transfer = self
            .consume(document, request, Direction::Download)
            .ok_or_else(|| unavailable(Stage::GetObject))?;
        let ApprovedFile::Download(file) = transfer.file else { return Err(unavailable(Stage::GetObject)); };
        let target = transfer.target;
        let regional = transfer.client.in_region(&target.region, Stage::GetObject)?;
        let result = regional.download(&target.bucket, &target.key, &file, &transfer.closed).await;
        self.record_expiration(document, &target.session_id, &result);
        result
    }
}

fn is_login_required<T>(result: &Result<T, Error>) -> bool {
    result.as_ref().is_err_and(|error| error.failure == Failure::SsoLoginRequired)
}

fn mark_expired<T>(tab: &mut Tab, result: &Result<T, Error>) {
    if is_login_required(result) {
        tab.expired = true;
        tab.selection = None;
    }
}

fn bucket_verified(tab: &Tab, target: &Target) -> bool {
    !tab.expired && !tab.listing && tab.verified_bucket.as_ref()
        .is_some_and(|(bucket, region)| *bucket == target.bucket && *region == target.region)
}

struct Transfer {
    client: Arc<AwsS3>,
    closed: Arc<watch::Sender<bool>>,
    target: Target,
    file: ApprovedFile,
}

async fn until_closed<T>(
    closed: &watch::Receiver<bool>,
    stage: Stage,
    operation: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    let mut closed = closed.clone();
    tokio::select! {
        biased;
        _ = closed.wait_for(|closed| *closed) => Err(unavailable(stage)),
        result = operation => result,
    }
}

fn unavailable(stage: Stage) -> Error {
    Error::at(stage, Failure::Request)
}

fn verified_selection(directory: &std::path::Path, target_id: &str) -> Result<VerifiedSsoSelection, Error> {
    let (scope, account) = with_saved_method(directory, target_id, |method, account| match method {
        MethodRef::Ssm { profile, region, .. } => Ok((Scope::new(profile.to_owned(), region.to_owned()), account.map(str::to_owned))),
        MethodRef::Local { .. } | MethodRef::Ssh { .. } => Err("SSO 연결이 아닙니다".into()),
    })
    .map_err(|_| Error::at(Stage::Profile, Failure::InvalidSsoProfile))?;
    let account = account.ok_or_else(|| Error::at(Stage::Profile, Failure::RetestRequired))?;
    let scope = scope.map_err(|error| Error::at(Stage::Profile, match error {
        CliError::InvalidRegion => Failure::InvalidRegion,
        _ => Failure::InvalidSsoProfile,
    }))?;
    let config = scope.direct_sso_config()
        .map_err(|_| Error::at(Stage::Profile, Failure::InvalidSsoProfile))?;
    if config.account != account {
        return Err(Error::at(Stage::Profile, Failure::AccountMismatch));
    }
    Ok(VerifiedSsoSelection { scope, config })
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Target {
    session_id: String,
    bucket: String,
    region: String,
    key: String,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum Direction {
    Upload,
    Download,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileSelection {
    #[serde(flatten)]
    target: Target,
    direction: Direction,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovedTransfer {
    #[serde(flatten)]
    target: Target,
    approval_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApprovalId {
    session_id: String,
    approval_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OpenRequest {
    id: Uuid,
    target_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LoginRequest { id: Uuid }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TabId {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BucketPageRequest {
    session_id: String,
    token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ObjectPageRequest {
    session_id: String,
    token: Option<String>,
    bucket: String,
    region: String,
    prefix: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PreviousFilesRequest {
    session_id: String,
    bucket: String,
    region: String,
    prefix: String,
}

#[derive(Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum LocalFileApproval {
    Approved { approval_id: String },
    Cancelled,
    Denied,
}

#[derive(Serialize)]
#[serde(untagged)]
enum S3Result<T> {
    Success { ok: bool, value: T },
    Failure { ok: bool, error: Error },
}

impl<T> From<Result<T, Error>> for S3Result<T> {
    fn from(result: Result<T, Error>) -> Self {
        match result {
            Ok(value) => Self::Success { ok: true, value },
            Err(error) => Self::Failure { ok: false, error },
        }
    }
}

async fn select_file(
    webview: tauri::Webview<impl tauri::Runtime>,
    document: &Document,
    request: FileSelection,
) -> LocalFileApproval {
    let state = webview.state::<StorageIpc>();
    let FileSelection { target, direction } = request;
    let session_id = target.session_id.clone();
    let action = match direction {
        Direction::Upload => "업로드",
        Direction::Download => "다운로드",
    };
    let title = format!("S3 {action} · {} / {:?} · {}", target.bucket, target.key, target.region);
    let Some(id) = state.begin_selection(document, target, direction) else {
        return LocalFileApproval::Denied;
    };
    let Ok((_, closed)) = state.client(document, &session_id, Stage::Profile) else {
        return LocalFileApproval::Denied;
    };
    let window = webview.window();
    let replace = if direction == Direction::Download {
        match choose_replace(window.clone(), closed.clone()).await {
            Some(replace) => replace,
            None => return state.finish_selection(document, &session_id, id, None),
        }
    } else { false };
    let picker_direction = if replace { Direction::Upload } else { direction };
    let selected = match pick_local_file(window.clone(), title.clone(), picker_direction, closed.clone()).await {
        Some(path) => {
            let display = format!("{path:?}");
            let selected = tokio::task::spawn_blocking(move || {
                let file = ApprovedFile::open(&path, direction)?;
                if let ApprovedFile::Download(target) = &file && target.replaces_existing() != replace {
                    return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "download destination changed"));
                }
                Ok(file)
            }).await.unwrap_or_else(|error| Err(std::io::Error::other(error)));
            if matches!(&selected, Ok(ApprovedFile::Download(target)) if target.replaces_existing()) {
                let prompt = format!("{title}\n\n기존 파일을 교체하시겠습니까?\n{display}");
                if confirm_replace(window, prompt, closed).await { Some(selected) } else { None }
            } else { Some(selected) }
        }
        None => None,
    };
    state.finish_selection(document, &session_id, id, selected)
}

#[cfg(windows)]
#[path = "dialogs_windows.rs"]
mod dialogs_windows;
#[cfg(windows)]
use dialogs_windows::{choose_replace, confirm_replace, pick as pick_local_file};

#[cfg(not(any(windows, target_os = "linux")))]
async fn pick_local_file<R: tauri::Runtime>(window: tauri::Window<R>, title: String, direction: Direction, mut closed: watch::Receiver<bool>) -> Option<PathBuf> {
    let confirmation = rfd::AsyncMessageDialog::new().set_parent(&window)
        .set_title("S3 파일 선택").set_description(&title).set_buttons(rfd::MessageButtons::OkCancel);
    let confirmed = tokio::select! {
        biased;
        _ = closed.wait_for(|closed| *closed) => return None,
        result = confirmation.show() => result == rfd::MessageDialogResult::Ok,
    };
    if !confirmed { return None; }
    let dialog = rfd::AsyncFileDialog::new().set_parent(&window).set_title("S3 파일 선택");
    tokio::select! {
        biased;
        _ = closed.wait_for(|closed| *closed) => None,
        result = async { match direction {
            Direction::Upload => dialog.pick_file().await,
            Direction::Download => dialog.save_file().await,
        }} => result.map(|file| file.path().to_owned()),
    }
}

#[cfg(target_os = "linux")]
async fn pick_local_file<R: tauri::Runtime>(window: tauri::Window<R>, title: String, direction: Direction, mut closed: watch::Receiver<bool>) -> Option<PathBuf> {
    use gtk::{ButtonsType, DialogFlags, FileChooserAction, FileChooserDialog, MessageDialog, MessageType, ResponseType, prelude::*};
    let (answer, selected) = tokio::sync::oneshot::channel();
    let owner = window.clone();
    window.run_on_main_thread(move || {
        let Ok(parent) = owner.gtk_window() else { return; };
        gtk::glib::MainContext::default().spawn_local(async move {
            // 소유 창에 대상을 표시하고 GTK가 반환한 PathBuf를 그대로 사용한다.
            let confirmation = MessageDialog::new(Some(&parent), DialogFlags::MODAL | DialogFlags::DESTROY_WITH_PARENT,
                MessageType::Question, ButtonsType::None, &title);
            confirmation.add_button("취소", ResponseType::Cancel);
            confirmation.add_button("파일 선택", ResponseType::Accept);
            let confirmed = tokio::select! {
                biased;
                _ = closed.wait_for(|closed| *closed) => false,
                answer = confirmation.run_future() => answer == ResponseType::Accept,
            };
            confirmation.close();
            if !confirmed { return; }

            let (action, label) = match direction {
                Direction::Upload => (FileChooserAction::Open, "열기"),
                Direction::Download => (FileChooserAction::Save, "저장"),
            };
            let picker = FileChooserDialog::with_buttons(Some(&title), Some(&parent), action,
                &[("취소", ResponseType::Cancel), (label, ResponseType::Accept)]);
            picker.set_modal(true);
            picker.set_destroy_with_parent(true);
            picker.set_local_only(true);
            picker.set_do_overwrite_confirmation(false);
            let path = tokio::select! {
                biased;
                _ = closed.wait_for(|closed| *closed) => None,
                answer = picker.run_future() => (answer == ResponseType::Accept).then(|| picker.filename()).flatten(),
            };
            picker.close();
            let _ = answer.send(path);
        });
    }).ok()?;
    selected.await.ok().flatten()
}

#[cfg(not(any(windows, target_os = "linux")))]
async fn choose_replace<R: tauri::Runtime>(window: tauri::Window<R>, mut closed: watch::Receiver<bool>) -> Option<bool> {
    let dialog = rfd::AsyncMessageDialog::new().set_parent(&window).set_title("다운로드 위치")
        .set_description("기존 파일을 교체하시겠습니까?\n예: 기존 파일 선택 / 아니요: 새 파일 저장")
        .set_buttons(rfd::MessageButtons::YesNoCancel);
    tokio::select! {
        biased;
        _ = closed.wait_for(|value| *value) => None,
        answer = dialog.show() => match answer {
            rfd::MessageDialogResult::Yes => Some(true),
            rfd::MessageDialogResult::No => Some(false),
            _ => None,
        },
    }
}

#[cfg(target_os = "linux")]
async fn choose_replace<R: tauri::Runtime>(window: tauri::Window<R>, closed: watch::Receiver<bool>) -> Option<bool> {
    use gtk::ResponseType;
    match download_dialog(window, "다운로드 위치", "다운로드 위치".into(), true, closed).await? {
        ResponseType::Yes => Some(true),
        ResponseType::No => Some(false),
        _ => None,
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
async fn confirm_replace<R: tauri::Runtime>(window: tauri::Window<R>, prompt: String, mut closed: watch::Receiver<bool>) -> bool {
    let dialog = rfd::AsyncMessageDialog::new().set_parent(&window).set_title("다운로드 파일 교체")
        .set_description(prompt).set_buttons(rfd::MessageButtons::OkCancel);
    tokio::select! {
        biased;
        _ = closed.wait_for(|value| *value) => false,
        answer = dialog.show() => answer == rfd::MessageDialogResult::Ok,
    }
}

#[cfg(target_os = "linux")]
async fn confirm_replace<R: tauri::Runtime>(window: tauri::Window<R>, prompt: String, closed: watch::Receiver<bool>) -> bool {
    download_dialog(window, "다운로드 파일 교체", prompt, false, closed).await == Some(gtk::ResponseType::Ok)
}

#[cfg(target_os = "linux")]
async fn download_dialog<R: tauri::Runtime>(window: tauri::Window<R>, title: &'static str, prompt: String, choose_mode: bool,
    mut closed: watch::Receiver<bool>) -> Option<gtk::ResponseType> {
    use gtk::{ButtonsType, DialogFlags, MessageDialog, MessageType, ResponseType, prelude::*};
    let (answer, result) = tokio::sync::oneshot::channel();
    let owner = window.clone();
    if window.run_on_main_thread(move || {
        let Ok(parent) = owner.gtk_window() else { return; };
        gtk::glib::MainContext::default().spawn_local(async move {
            let dialog = MessageDialog::new(Some(&parent), DialogFlags::MODAL | DialogFlags::DESTROY_WITH_PARENT,
                MessageType::Question, if choose_mode { ButtonsType::None } else { ButtonsType::OkCancel }, &prompt);
            dialog.set_title(title);
            if choose_mode {
                dialog.add_button("취소", ResponseType::Cancel);
                dialog.add_button("새 파일 저장", ResponseType::No);
                dialog.add_button("기존 파일 교체", ResponseType::Yes);
            }
            dialog.set_default_response(ResponseType::Cancel);
            let confirmed = tokio::select! {
                biased;
                _ = closed.wait_for(|value| *value) => ResponseType::Cancel,
                result = dialog.run_future() => result,
            };
            dialog.close();
            let _ = answer.send(confirmed);
        });
    }).is_err() { return None; }
    result.await.ok()
}

pub(crate) fn dispatch<R: tauri::Runtime>(invoke: Invoke<R>) -> bool {
    let Invoke {
        message,
        resolver,
        acl,
    } = invoke;
    let name = message.command();
    let webview = message.webview_ref().clone();
    if acl.is_none() || webview.label() != "main" || webview.window_ref().label() != "main" {
        return false;
    }
    // 비동기 처리를 예약하기 전에 실제 IPC 발신자의 문서 세대를 읽는다.
    let document = webview.state::<StorageIpc>().document(webview.label());
    macro_rules! handle {
        ($command:literal, $request:ty, $run:expr) => {{
            let item = CommandItem {
                name: $command,
                key: "request",
                plugin: None,
                message: &message,
                acl: &acl,
            };
            let request = match <$request>::from_command(item) {
                Ok(request) => request,
                Err(error) => {
                    resolver.invoke_error(error);
                    return true;
                }
            };
            resolver.respond_async($run(webview, document, request));
        }};
    }
    match name {
        "storage_open" => {
            let item = CommandItem { name: "storage_open", key: "request", plugin: None, message: &message, acl: &acl };
            let request = match OpenRequest::from_command(item) {
                Ok(request) => request,
                Err(error) => { resolver.invoke_error(error); return true; }
            };
            let prepared = webview.path().app_config_dir().map_err(|_| unavailable(Stage::Profile))
                .and_then(|directory| webview.state::<StorageIpc>().reserve_open(&document, request.id).map(|cancel| (directory, cancel)));
            resolver.respond_async(async move {
                let result = match prepared {
                    Ok((directory, cancel)) => webview.state::<StorageIpc>().open_with(document, request, directory, cancel, |region, config| async move {
                        AwsS3::for_sso_profile(&region, config).await
                    }).await,
                    Err(error) => Err(error),
                };
                Ok(S3Result::from(result))
            });
        }
        "storage_relogin" => handle!(
            "storage_relogin", LoginRequest,
            async |webview: tauri::Webview<R>, document, request: LoginRequest| {
                let result = match webview.path().app_config_dir() {
                    Ok(directory) => webview.state::<StorageIpc>().relogin(&document, request.id, directory).await,
                    Err(_) => Err(unavailable(Stage::Profile)),
                };
                Ok(S3Result::from(result))
            }
        ),
        "storage_list_buckets" => handle!(
            "storage_list_buckets",
            BucketPageRequest,
            async |webview: tauri::Webview<R>, document, request| {
                Ok(S3Result::from(
                    webview
                        .state::<StorageIpc>()
                        .list_buckets(&document, request)
                        .await,
                ))
            }
        ),
        "storage_list_objects" => handle!(
            "storage_list_objects",
            ObjectPageRequest,
            async |webview: tauri::Webview<R>, document, request| {
                Ok(S3Result::from(
                    webview
                        .state::<StorageIpc>()
                        .list_objects(&document, request)
                        .await,
                ))
            }
        ),
        "storage_count_previous_files" => handle!(
            "storage_count_previous_files",
            PreviousFilesRequest,
            async |webview: tauri::Webview<R>, document, request| {
                Ok(S3Result::from(
                    webview.state::<StorageIpc>().count_previous_files(&document, request).await,
                ))
            }
        ),
        "storage_select_local_file" => handle!(
            "storage_select_local_file",
            FileSelection,
            async |webview, document, request| {
                Ok(select_file(webview, &document, request).await)
            }
        ),
        "storage_upload" => handle!(
            "storage_upload",
            ApprovedTransfer,
            async |webview: tauri::Webview<R>, document, request| {
                Ok(S3Result::from(
                    webview
                        .state::<StorageIpc>()
                        .upload(&document, request)
                        .await,
                ))
            }
        ),
        "storage_download" => handle!(
            "storage_download",
            ApprovedTransfer,
            async |webview: tauri::Webview<R>, document, request| {
                Ok(S3Result::from(
                    webview
                        .state::<StorageIpc>()
                        .download(&document, request)
                        .await,
                ))
            }
        ),
        "storage_close" => handle!(
            "storage_close",
            TabId,
            async |webview: tauri::Webview<R>, document, request: TabId| {
                Ok(webview
                    .state::<StorageIpc>()
                    .close(&document, &request.session_id))
            }
        ),
        "storage_cancel_approval" => handle!(
            "storage_cancel_approval",
            ApprovalId,
            async |webview: tauri::Webview<R>, document, request: ApprovalId| {
                Ok(webview
                    .state::<StorageIpc>()
                    .cancel_approval(&document, &request))
            }
        ),
        _ => return false,
    }
    true
}

#[cfg(test)]
#[path = "versions_ipc_tests.rs"]
mod versions_tests;

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{client, replay, response},
        *,
    };
    use aws_sdk_s3::primitives::SdkBody;

    const KEY: &str = "실험//../한글 +%2F.txt";

    fn target(session: &Session) -> Target {
        Target {
            session_id: session.session_id.clone(),
            bucket: "selected-bucket".into(),
            region: session.region.clone(),
            key: KEY.into(),
        }
    }

    #[test]
    fn storage_revocation_does_not_hold_registry_while_signaling() {
        for navigate in [false, true] {
            let state = StorageIpc::default();
            let replay = replay([response(200, "<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>")]);
            let doc = state.document("target");
            let peer_doc = state.document("peer");
            let first = state.insert(doc.clone(), "test".into(), "eu-west-1".into(), client(&replay)).unwrap();
            let peer = state.insert(peer_doc.clone(), "test".into(), "eu-west-1".into(), client(&replay)).unwrap();
            let (_, closed) = state.client(&doc, &first.session_id, Stage::ListBuckets).unwrap();
            let guard = closed.borrow();
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    if navigate { state.navigate("target"); }
                    else { assert!(state.close(&doc, &first.session_id)); }
                });
                let observed = (|| {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    while std::time::Instant::now() < deadline {
                        if let Ok(registry) = state.0.try_lock() && !registry.tabs.contains_key(&first.session_id) {
                            drop(registry);
                            return tauri::async_runtime::block_on(state.list_buckets(&peer_doc, BucketPageRequest {
                                session_id: peer.session_id.clone(), token: None,
                            })).is_ok();
                        }
                        std::thread::yield_now();
                    }
                    false
                })();
                drop(guard);
                worker.join().unwrap();
                assert!(observed, "peer blocked during revocation: navigate={navigate}");
            });
            assert!(*closed.borrow());
        }
    }

    #[test]
    fn storage_close_ipc_stops_inflight_download_without_replacing_file() {
        use serde_json::json;
        use tauri::{ipc::{CallbackFn, InvokeBody, InvokeResponse}, test, utils::acl::ExecutionContext, webview::InvokeRequest};
        use super::super::tests::{PausedStream, object_response};

        let mut context = test::mock_context(test::noop_assets());
        for command in ["storage_download", "storage_close"] {
            context.runtime_authority_mut().__allow_command(command.into(), ExecutionContext::Local);
        }
        let app = test::mock_builder().manage(StorageIpc::default()).invoke_handler(dispatch).build(context).unwrap();
        let window = tauri::WebviewWindowBuilder::new(&app, "main", Default::default()).build().unwrap();
        let request = |command: &str, request| InvokeRequest {
            cmd: command.into(), callback: CallbackFn(0), error: CallbackFn(1),
            url: if cfg!(windows) { "http://tauri.localhost" } else { "tauri://localhost" }.parse().unwrap(),
            body: InvokeBody::Json(json!({ "request": request })), headers: Default::default(),
            invoke_key: test::INVOKE_KEY.into(),
        };
        let (started, paused) = tokio::sync::oneshot::channel();
        let replay = replay([object_response(3, None, SdkBody::from_body_1_x(PausedStream::new(started)))]);
        let state = app.state::<StorageIpc>();
        let document = state.document("main");
        let session = state.insert(document.clone(), "fixture".into(), "eu-west-1".into(), client(&replay)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) { " 한글.txt" } else { " 한글\n.txt " };
        let path = dir.path().join(name);
        std::fs::write(&path, b"original").unwrap();
        let approval = state.begin_selection(&document, target(&session), Direction::Download).unwrap();
        state.finish_selection(&document, &session.session_id, approval.clone(), Some(ApprovedFile::open(&path, Direction::Download)));
        let (send, received) = std::sync::mpsc::channel();
        let webview: &tauri::Webview<test::MockRuntime> = window.as_ref();
        webview.clone().on_message(request("storage_download", json!({
            "sessionId": session.session_id, "bucket": "selected-bucket", "region": session.region, "key": KEY, "approvalId": approval,
        })), Box::new(move |_, _, reply, _, _| { send.send(reply).unwrap(); }));
        let timeout = std::time::Duration::from_secs(5);
        tauri::async_runtime::block_on(async { tokio::time::timeout(timeout, paused).await.unwrap().unwrap(); });
        let close = || request("storage_close", json!({ "sessionId": session.session_id }));
        assert!(test::get_ipc_response(&window, close()).unwrap().deserialize::<bool>().unwrap());
        let InvokeResponse::Ok(body) = received.recv_timeout(timeout).unwrap() else { panic!("다운로드 IPC 오류"); };
        assert_eq!(body.deserialize::<serde_json::Value>().unwrap(), json!({
            "ok": false, "error": { "stage": "downloadBody", "failure": { "kind": "request" }, "bytes": 1 }
        }));
        assert!(!test::get_ipc_response(&window, close()).unwrap().deserialize::<bool>().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn storage_open_uses_only_current_saved_direct_sso() {
        const CHILD: &str = "ZZABBA_S01_OPEN_FIXTURE";
        let Some(root) = std::env::var_os(CHILD).map(PathBuf::from) else {
            let root = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "storage::ipc::tests::storage_open_uses_only_current_saved_direct_sso"])
                .env(CHILD, root.path())
                .env("HOME", root.path().join("home"))
                .env("USERPROFILE", root.path().join("profile"))
                .env("AWS_CONFIG_FILE", root.path().join("config"))
                .env("AWS_SHARED_CREDENTIALS_FILE", root.path().join("credentials"))
                .output().unwrap();
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stdout));
            assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
            return;
        };
        use serde_json::json;
        let directory = root.join("app");
        std::fs::create_dir(&directory).unwrap();
        let group = json!({"id": "team", "displayName": "Team"});
        let target = json!({
            "id": "saved-sso", "groupId": "team", "displayName": "SSO", "defaultMethod": "ssm",
            "ssm": {"profile": "chosen team", "region": "eu-west-1", "instanceId": "i-01234567"}
        });
        let file = directory.join("connections.json");
        std::fs::write(&file, serde_json::to_vec(&json!({
            "version": 1,
            "groups": [{"revision": 1, "profile": group}],
            "targets": [{"revision": 1, "profile": target}]
        })).unwrap()).unwrap();
        let config = root.join("config");
        let write_config = |account| std::fs::write(&config, format!(concat!(
            "[profile \"chosen team\"]\nsso_session = fixture\nsso_account_id = {}\nsso_role_name = ReadOnly\n",
            "[sso-session fixture]\nsso_region = us-east-1\nsso_start_url = https://fixture.invalid/start\n",
        ), account)).unwrap();
        write_config("123456789012");
        assert_eq!(verified_selection(&directory, "saved-sso").err().unwrap().failure, Failure::RetestRequired);
        let input = json!({"version": 1, "groupRevision": 1, "targetRevision": 1, "group": group, "target": target});
        let mut forged = input.clone();
        forged["target"]["ssm"]["testedAccountId"] = json!("000000000000");
        assert!(serde_json::from_value::<crate::connections::profile::Draft>(forged).is_err());
        crate::connections::probe::save_ssm_fixture(&directory, input, "123456789012");
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        assert_eq!(saved["targets"][0]["tested_account_id"], "123456789012");
        write_config("000000000000");
        assert_eq!(verified_selection(&directory, "saved-sso").err().unwrap().failure, Failure::AccountMismatch);
        write_config("123456789012");

        tauri::async_runtime::block_on(async {
            let state = StorageIpc::default();
            let document = state.document("main");
            let request = |target_id: &str| OpenRequest { id: Uuid::new_v4(), target_id: target_id.to_owned() };
            let home = crate::ssm::user_home().unwrap();
            assert_eq!(home, root.join(if cfg!(windows) { "profile" } else { "home" }));
            let token = home.join(crate::ssm::sso_cache_entry("fixture"));
            std::fs::create_dir_all(token.parent().unwrap()).unwrap();
            std::fs::write(&token, r#"{"accessToken":"fixture-token","expiresAt":"2099-01-01T00:00:00Z"}"#).unwrap();
            let clock = super::super::tests::Clock::default();
            let head = |status, region| http::Response::builder().status(status)
                .header("x-amz-bucket-region", region).body(SdkBody::empty()).unwrap().try_into().unwrap();
            let replay = replay([
                super::super::tests::role_credentials("WRONG", &clock, 60),
                super::super::tests::caller_identity("000000000000"),
                super::super::tests::role_credentials("FIRST", &clock, 60),
                super::super::tests::caller_identity("123456789012"),
                response(200, "<ListAllMyBucketsResult><Buckets><Bucket><Name>selected-bucket</Name><BucketRegion>us-west-2</BucketRegion></Bucket></Buckets></ListAllMyBucketsResult>"),
                head(301, "us-west-2"),
                response(403, SdkBody::empty()),
                head(403, "us-west-2"),
                response(200, "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>false</IsTruncated></ListBucketResult>"),
                head(403, "us-west-2"),
                response(403, "<Error><Code>AccessDenied</Code></Error>"),
                super::super::tests::role_credentials("SECOND", &clock, 120),
                response(200, "<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>"),
            ]);
            let open = |request: OpenRequest| {
              let cancel = state.reserve_open(&document, request.id).unwrap();
              state.open_with(document.clone(), request, directory.clone(), cancel, |region, config| {
                let transport = super::super::SharedHttpClient::new(replay.clone());
                let clock = super::super::SharedTimeSource::new(clock.clone());
                async move { AwsS3::verified(&region, config, crate::ssm::user_home().unwrap(), transport, clock).await }
              })
            };
            assert_eq!(open(request("saved-sso")).await.err().unwrap().failure, Failure::AccountMismatch);
            assert!(state.0.lock().unwrap().tabs.is_empty());
            let session = open(request("saved-sso")).await.unwrap();
            let list = || state.list_buckets(&document, BucketPageRequest { session_id: session.session_id.clone(), token: None });
            assert_eq!(list().await.unwrap().rows, [BucketRow { name: "selected-bucket".into(), region: "us-west-2".into() }]);
            let objects = |region: &str| state.list_objects(&document, ObjectPageRequest {
                session_id: session.session_id.clone(), bucket: "selected-bucket".into(),
                region: region.into(), prefix: "실험/".into(), token: None,
            });
            assert_eq!(objects("eu-west-1").await.unwrap_err().failure, Failure::BucketRegionMismatch { region: "us-west-2".into() });
            let regional = || Target { region: "us-west-2".into(), ..self::target(&session) };
            assert!(state.begin_selection(&document, regional(), Direction::Upload).is_none());
            let missing_region = objects("us-west-2").await.unwrap_err();
            assert_eq!(missing_region.stage, Stage::HeadBucket);
            assert!(matches!(missing_region.failure, Failure::Service { status: 403, .. }));
            assert!(state.begin_selection(&document, regional(), Direction::Upload).is_none());
            assert!(objects("us-west-2").await.unwrap().rows.is_empty());
            let approval = state.begin_selection(&document, regional(), Direction::Upload).unwrap();
            assert!(matches!(state.finish_selection(&document, &session.session_id, approval, None), LocalFileApproval::Cancelled));
            let denied = objects("us-west-2").await.unwrap_err();
            assert_eq!(denied.stage, Stage::ListObjects);
            assert_eq!(denied.failure, Failure::Service { status: 403, code: Some("AccessDenied".into()) });
            assert!(state.begin_selection(&document, regional(), Direction::Upload).is_none());
            clock.advance(61);
            assert!(list().await.unwrap().rows.is_empty());
            let requests: Vec<_> = replay.actual_requests().collect();
            assert_eq!(requests.len(), 13);
            for index in [0, 2, 11] {
                let uri: http::Uri = requests[index].uri().parse().unwrap();
                assert_eq!(uri.host(), Some("portal.sso.us-east-1.amazonaws.com"));
                assert_eq!(uri.path(), "/federation/credentials");
                let mut pairs: Vec<_> = uri.query().unwrap().split('&').collect();
                pairs.sort_unstable();
                assert_eq!(pairs, ["account_id=123456789012", "role_name=ReadOnly"]);
                assert_eq!(requests[index].headers().get("x-amz-sso_bearer_token"), Some("fixture-token"));
            }
            for (index, scheme, region, key) in [(1, "sts", "eu-west-1", "WRONG"), (3, "sts", "eu-west-1", "FIRST"),
                (4, "s3", "eu-west-1", "FIRST"), (5, "s3", "eu-west-1", "FIRST"),
                (6, "s3", "us-west-2", "FIRST"), (7, "s3", "us-west-2", "FIRST"),
                (8, "s3", "us-west-2", "FIRST"), (9, "s3", "us-west-2", "FIRST"),
                (10, "s3", "us-west-2", "FIRST"), (12, "s3", "eu-west-1", "SECOND")] {
                assert!(requests[index].uri().starts_with(&format!("https://{scheme}.{region}.amazonaws.com/")));
                let authorization = requests[index].headers().get("authorization").unwrap();
                assert!(authorization.contains(&format!("Credential={key}/")));
                assert!(authorization.contains(&format!("/{region}/{scheme}/aws4_request")));
            }
            for (index, method) in [(5, "HEAD"), (6, "HEAD"), (7, "HEAD"), (8, "GET"), (9, "HEAD"), (10, "GET")] {
                assert_eq!(requests[index].method(), method);
                let uri: http::Uri = requests[index].uri().parse().unwrap();
                assert!(uri.path().starts_with("/selected-bucket"));
                if method == "GET" {
                    let query = uri.query().unwrap();
                    assert!(query.contains("list-type=2"));
                    assert!(query.contains("prefix=%EC%8B%A4%ED%97%98%2F"));
                }
            }
            assert_eq!(std::fs::read_to_string(&token).unwrap(), r#"{"accessToken":"fixture-token","expiresAt":"2099-01-01T00:00:00Z"}"#);
            assert!(Uuid::parse_str(&session.session_id).is_ok());
            assert_eq!(serde_json::to_value(&session).unwrap(), json!({
                "sessionId": session.session_id, "profile": "chosen team", "region": "eu-west-1",
                "endpoint": "https://s3.eu-west-1.amazonaws.com"
            }));
            let missing = open(request("chosen team")).await.err().unwrap();
            assert_eq!(missing.failure, Failure::InvalidSsoProfile);

            let opening = request("saved-sso");
            let id = opening.id;
            let cancel = state.reserve_open(&document, id).unwrap();
            let work = state.open_with(document.clone(), opening, directory.clone(), cancel, |_, _| std::future::pending());
            let (result, ()) = tokio::join!(work, async {
                assert!(!state.close(&state.document("other"), &id.to_string()));
                assert!(state.close(&document, &id.to_string()));
            });
            assert!(result.is_err());
            assert!(!state.close(&document, &id.to_string()));

            // 레지스트리에서 분리됐지만 잠금 밖 취소 신호가 아직 전달되지 않은 순서다.
            let opening = OpenRequest { id, target_id: "saved-sso".into() };
            let previous = state.reserve_open(&document, id).unwrap();
            let detached = state.0.lock().unwrap().openings.remove(&id).unwrap();
            let successor = state.reserve_open(&document, id).unwrap();
            let stale = state.open_with(document.clone(), opening, directory.clone(), previous, |_, _| async {
                Err(Error::at(Stage::Identity, Failure::SsoLoginRequired))
            }).await.err().unwrap();
            assert_eq!(stale.failure, Failure::Request);
            assert!(Arc::ptr_eq(&state.0.lock().unwrap().openings[&id].cancel, &successor));
            detached.cancel.cancel();
            assert!(!successor.is_cancelled());
            assert!(state.close(&document, &id.to_string()));

            clock.advance(120);
            std::fs::remove_file(&token).unwrap();
            assert_eq!(list().await.unwrap_err().failure, Failure::SsoLoginRequired);
            let session_id = Uuid::parse_str(&session.session_id).unwrap();
            let unexpected = |_, _| async { panic!("재로그인 권한 없음"); };
            assert!(state.relogin_with(&state.document("other"), session_id, directory.clone(), unexpected).await.is_err());
            state.relogin_with(&document, session_id, directory.clone(), |current, _| async move {
                assert_eq!(current.scope.profile(), "chosen team");
                assert_eq!(current.config.account, "123456789012");
                Ok(())
            }).await.unwrap();
            assert!(state.relogin_with(&document, session_id, directory.clone(), unexpected).await.is_err());
            assert!(state.begin_selection(&document, regional(), Direction::Upload).is_none());

            let expired = request("saved-sso");
            let expired_id = expired.id;
            assert_eq!(open(expired).await.err().unwrap().failure, Failure::SsoLoginRequired);
            write_config("000000000000");
            assert_eq!(state.relogin_with(&document, expired_id, directory.clone(), unexpected).await.unwrap_err().failure, Failure::AccountMismatch);
            write_config("123456789012");
            assert!(state.relogin_with(&document, expired_id, directory.clone(), unexpected).await.is_err());
            assert_eq!(replay.actual_requests().count(), 13);

            std::fs::write(root.join("credentials"), "[chosen team]\naws_access_key_id=fixture\n").unwrap();
            let changed = open(request("saved-sso")).await.err().unwrap();
            assert_eq!(changed.failure, Failure::InvalidSsoProfile);
            state.navigate("main");
            assert!(!state.close(&state.document("main"), &session.session_id));
            assert!(state.reserve_open(&document, Uuid::new_v4()).is_err());
        });
    }

    #[tokio::test]
    async fn 다른_탭과_문서의_전송_및_실패한_전송의_재사용을_거부한다() {
        let reply = replay([
            response(500, "<Error><Code>InternalError</Code></Error>"),
            response(200, SdkBody::empty()),
        ]);
        let state = StorageIpc::default();
        let doc = state.document("main");
        let first = state
            .insert(
                doc.clone(),
                "first".into(),
                "eu-west-1".into(),
                client(&reply),
            )
            .unwrap();
        let second = state
            .insert(
                doc.clone(),
                "second".into(),
                "eu-west-1".into(),
                client(&reply),
            )
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("그대로.txt");
        std::fs::write(&file, b"payload").unwrap();
        let id = state
            .begin_selection(&doc, target(&first), Direction::Upload)
            .unwrap();
        assert!(matches!(
            state.finish_selection(&doc, &first.session_id, id.clone(), Some(ApprovedFile::open(&file, Direction::Upload))),
            LocalFileApproval::Approved { .. }
        ));
        let request = |target| ApprovedTransfer {
            target,
            approval_id: id.clone(),
        };
        assert!(
            state
                .upload(&state.document("other"), request(target(&first)))
                .await
                .is_err()
        );
        assert!(state.upload(&doc, request(target(&second))).await.is_err());
        for changed in [
            Target {
                bucket: "other-bucket".into(),
                ..target(&first)
            },
            Target {
                key: KEY.replace("//", "/"),
                ..target(&first)
            },
        ] {
            assert!(state.upload(&doc, request(changed)).await.is_err());
        }
        assert!(state.download(&doc, request(target(&first))).await.is_err());
        assert_eq!(reply.actual_requests().count(), 0);
        assert_eq!(
            state
                .upload(&doc, request(target(&first)))
                .await
                .unwrap_err()
                .stage,
            Stage::PutObject
        );
        assert!(state.upload(&doc, request(target(&first))).await.is_err());
        let requests: Vec<_> = reply.actual_requests().collect();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].uri(),
            "https://s3.eu-west-1.amazonaws.com/selected-bucket/%EC%8B%A4%ED%97%98//../%ED%95%9C%EA%B8%80%20%2B%252F.txt?x-id=PutObject"
        );
        assert_eq!(std::fs::read(file).unwrap(), b"payload");
    }

    #[tokio::test]
    async fn 취소와_닫기는_승인을_폐기하고_다른_탭의_목록을_살려둔다() {
        let reply = replay([response(
            200,
            "<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>",
        )]);
        let state = StorageIpc::default();
        let doc = state.document("main");
        let first = state
            .insert(
                doc.clone(),
                "test".into(),
                "eu-west-1".into(),
                client(&reply),
            )
            .unwrap();
        let second = state
            .insert(
                doc.clone(),
                "test".into(),
                "eu-west-1".into(),
                client(&reply),
            )
            .unwrap();
        let cancelled = state
            .begin_selection(&doc, target(&first), Direction::Download)
            .unwrap();
        assert!(matches!(
            state.finish_selection(&doc, &first.session_id, cancelled, None),
            LocalFileApproval::Cancelled
        ));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("selected.txt");
        let id = state
            .begin_selection(&doc, target(&first), Direction::Download)
            .unwrap();
        state.finish_selection(&doc, &first.session_id, id.clone(), Some(ApprovedFile::open(&file, Direction::Download)));
        assert!(state.cancel_approval(
            &doc,
            &ApprovalId {
                session_id: first.session_id.clone(),
                approval_id: id.clone()
            }
        ));
        assert!(
            state
                .download(
                    &doc,
                    ApprovedTransfer {
                        target: target(&first),
                        approval_id: id
                    }
                )
                .await
                .is_err()
        );
        let pending = state
            .begin_selection(&doc, target(&first), Direction::Download)
            .unwrap();
        let (_, closed) = state
            .client(&doc, &first.session_id, Stage::ListBuckets)
            .unwrap();
        assert!(state.close(&doc, &first.session_id));
        assert!(*closed.borrow());
        assert!(matches!(
            state.finish_selection(&doc, &first.session_id, pending, Some(ApprovedFile::open(&file, Direction::Download))),
            LocalFileApproval::Denied
        ));
        let request = || BucketPageRequest {
            session_id: second.session_id.clone(),
            token: None,
        };
        assert!(
            state
                .list_buckets(&doc, request())
                .await
                .unwrap()
                .rows
                .is_empty()
        );
        state.navigate("main");
        assert!(state.list_buckets(&doc, request()).await.is_err());
        assert!(
            state
                .list_buckets(&state.document("main"), request())
                .await
                .is_err()
        );
        assert_eq!(reply.actual_requests().count(), 1);
    }
}
