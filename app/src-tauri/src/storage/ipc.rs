use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

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
    ssm::{CliError, Scope},
};

use super::{
    AwsS3, Downloaded, Error, Failure, ObjectRow, Page, Stage, VerifiedSsoSelection, check_bucket,
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

    async fn open(
        &self,
        document: Document,
        request: OpenRequest,
        directory: PathBuf,
    ) -> Result<Session, Error> {
        self.open_with(document, request, directory, |region, config| async move {
            AwsS3::for_sso_profile(&region, config).await
        }).await
    }

    async fn open_with<F: Future<Output = Result<AwsS3, Error>>>(
        &self, document: Document, request: OpenRequest, directory: PathBuf,
        verify: impl FnOnce(String, crate::ssm::DirectSsoConfig) -> F,
    ) -> Result<Session, Error> {
        if !self.0.lock().unwrap().active(&document) {
            return Err(unavailable(Stage::Profile));
        }
        let selection = tauri::async_runtime::spawn_blocking(move || {
            verified_selection(&directory, &request.target_id)
        })
        .await
        .map_err(|_| unavailable(Stage::Profile))??;
        let VerifiedSsoSelection { profile, region, config } = selection;
        let client = verify(region.clone(), config).await?;
        self.insert(document, profile, region, client)
            .ok_or_else(|| unavailable(Stage::Profile))
    }

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
        let session_id = Uuid::new_v4().to_string();
        let endpoint = client.endpoint().into();
        registry.tabs.insert(
            session_id.clone(),
            Tab {
                document,
                client: Arc::new(client),
                closed: Arc::new(watch::Sender::new(false)),
                selection: None,
            },
        );
        Some(Session {
            session_id,
            profile,
            region,
            endpoint,
        })
    }

    fn close(&self, document: &Document, session_id: &str) -> bool {
        let tab = {
            let mut registry = self.0.lock().unwrap();
            if registry.tab(document, session_id).is_none() { return false; }
            registry.tabs.remove(session_id).unwrap()
        };
        tab.closed.send_replace(true);
        true
    }

    pub(crate) fn navigate(&self, owner: &str) {
        let removed = {
            let mut registry = self.0.lock().unwrap();
            *registry.documents.entry(owner.into()).or_default() += 1;
            registry.tabs.extract_if(|_, tab| tab.document.owner == owner).map(|(_, tab)| tab).collect::<Vec<_>>()
        };
        for tab in removed { tab.closed.send_replace(true); }
    }

    pub(crate) fn shutdown(&self) {
        let removed = {
            let mut registry = self.0.lock().unwrap();
            registry.shutdown = true;
            std::mem::take(&mut registry.tabs)
        };
        for (_, tab) in removed {
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
        if tab.selection.is_some() {
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
    ) -> Result<Page<String>, Error> {
        let (client, closed) = self.client(document, &request.session_id, Stage::ListBuckets)?;
        until_closed(
            &closed,
            Stage::ListBuckets,
            client.list_buckets(request.token.as_deref()),
        )
        .await
    }

    async fn list_objects(
        &self,
        document: &Document,
        request: ObjectPageRequest,
    ) -> Result<Page<ObjectRow>, Error> {
        let (client, closed) = self.client(document, &request.session_id, Stage::ListObjects)?;
        until_closed(
            &closed,
            Stage::ListObjects,
            client.list_objects(&request.bucket, &request.prefix, request.token.as_deref()),
        )
        .await
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
        until_closed(
            &transfer.closed.subscribe(),
            Stage::PutObject,
            transfer.client.upload(
                &transfer.target.bucket,
                &transfer.target.key,
                file,
            ),
        )
        .await
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
        transfer
            .client
            .download(
                &transfer.target.bucket,
                &transfer.target.key,
                &file,
                &transfer.closed,
            )
            .await
    }
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
    Ok(VerifiedSsoSelection {
        profile: scope.profile().to_owned(),
        region: scope.region().to_owned(),
        config,
    })
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Target {
    session_id: String,
    bucket: String,
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
    target_id: String,
}

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
    let title = format!("S3 {action} · {} / {:?}", target.bucket, target.key);
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

#[cfg(not(target_os = "linux"))]
async fn pick_local_file<R: tauri::Runtime>(window: tauri::Window<R>, title: String, direction: Direction, _: watch::Receiver<bool>) -> Option<PathBuf> {
    let dialog = rfd::AsyncFileDialog::new().set_parent(&window).set_title(title);
    match direction {
        Direction::Upload => dialog.pick_file().await,
        Direction::Download => dialog.save_file().await,
    }.map(|file| file.path().to_owned())
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

#[cfg(not(target_os = "linux"))]
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

#[cfg(not(target_os = "linux"))]
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
        "storage_open" => handle!(
            "storage_open",
            OpenRequest,
            async |webview: tauri::Webview<R>, document, request| {
                let result = match webview.path().app_config_dir() {
                    Ok(directory) => webview.state::<StorageIpc>().open(document, request, directory).await,
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
            "sessionId": session.session_id, "bucket": "selected-bucket", "key": KEY, "approvalId": approval,
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
            let request = |id: &str| OpenRequest { target_id: id.to_owned() };
            let home = crate::ssm::user_home().unwrap();
            assert_eq!(home, root.join(if cfg!(windows) { "profile" } else { "home" }));
            let token = home.join(crate::ssm::sso_cache_entry("fixture"));
            std::fs::create_dir_all(token.parent().unwrap()).unwrap();
            std::fs::write(&token, r#"{"accessToken":"fixture-token","expiresAt":"2099-01-01T00:00:00Z"}"#).unwrap();
            let clock = super::super::tests::Clock::default();
            let replay = replay([
                super::super::tests::role_credentials("WRONG", &clock, 60),
                super::super::tests::caller_identity("000000000000"),
                super::super::tests::role_credentials("FIRST", &clock, 60),
                super::super::tests::caller_identity("123456789012"),
                response(200, "<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>"),
                super::super::tests::role_credentials("SECOND", &clock, 120),
                response(200, "<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>"),
            ]);
            let open = || state.open_with(document.clone(), request("saved-sso"), directory.clone(), |region, config| {
                let transport = super::super::SharedHttpClient::new(replay.clone());
                let clock = super::super::SharedTimeSource::new(clock.clone());
                async move { AwsS3::verified(&region, config, crate::ssm::user_home().unwrap(), transport, clock).await }
            });
            assert_eq!(open().await.err().unwrap().failure, Failure::AccountMismatch);
            assert!(state.0.lock().unwrap().tabs.is_empty());
            let session = open().await.unwrap();
            for elapsed in [0, 61] {
                clock.advance(elapsed);
                state.list_buckets(&document, BucketPageRequest { session_id: session.session_id.clone(), token: None }).await.unwrap();
            }
            let requests: Vec<_> = replay.actual_requests().collect();
            assert_eq!(requests.len(), 7);
            for index in [0, 2, 5] {
                let uri: http::Uri = requests[index].uri().parse().unwrap();
                assert_eq!(uri.host(), Some("portal.sso.us-east-1.amazonaws.com"));
                assert_eq!(uri.path(), "/federation/credentials");
                let mut pairs: Vec<_> = uri.query().unwrap().split('&').collect();
                pairs.sort_unstable();
                assert_eq!(pairs, ["account_id=123456789012", "role_name=ReadOnly"]);
                assert_eq!(requests[index].headers().get("x-amz-sso_bearer_token"), Some("fixture-token"));
            }
            for (index, scheme, region, key) in [(1, "sts", "eu-west-1", "WRONG"), (3, "sts", "eu-west-1", "FIRST"),
                (4, "s3", "eu-west-1", "FIRST"), (6, "s3", "eu-west-1", "SECOND")] {
                assert!(requests[index].uri().starts_with(&format!("https://{scheme}.{region}.amazonaws.com/")));
                assert!(requests[index].headers().get("authorization").unwrap().contains(&format!("Credential={key}/")));
            }
            assert_eq!(std::fs::read_to_string(&token).unwrap(), r#"{"accessToken":"fixture-token","expiresAt":"2099-01-01T00:00:00Z"}"#);
            assert!(Uuid::parse_str(&session.session_id).is_ok());
            assert_eq!(serde_json::to_value(&session).unwrap(), json!({
                "sessionId": session.session_id, "profile": "chosen team", "region": "eu-west-1",
                "endpoint": "https://s3.eu-west-1.amazonaws.com"
            }));
            let missing = state.open(document.clone(), request("chosen team"), directory.clone()).await.err().unwrap();
            assert_eq!(missing.failure, Failure::InvalidSsoProfile);

            std::fs::write(root.join("credentials"), "[chosen team]\naws_access_key_id=fixture\n").unwrap();
            let changed = state.open(document.clone(), request("saved-sso"), directory.clone()).await.err().unwrap();
            assert_eq!(changed.failure, Failure::InvalidSsoProfile);
            state.navigate("main");
            assert!(!state.close(&state.document("main"), &session.session_id));
            assert!(state.open(document, request("saved-sso"), directory).await.is_err());
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
