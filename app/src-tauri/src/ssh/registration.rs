mod dialogs;

use std::{path::{Path, PathBuf}, sync::{Arc, Mutex}};

use serde::{Deserialize, Serialize};
use ssh_key::{Fingerprint, HashAlg};
use tauri::{Manager, Webview, ipc::{CommandArg, CommandItem, Invoke}};
use crate::{connections::profile::Draft, terminal::Sessions};

use super::{ApprovedHost, Endpoint, Error, TrustStore, host_trust::{self, Checked, Confirm, Via}, trust::plain_public_key};

pub(crate) const COMMAND: &str = "verify_ssh_host";
pub(crate) const COMMIT: &str = "commit_ssh_host";
pub(crate) const CANCEL: &str = "cancel_ssh_host";

const HOST_CONFIRM_TITLE: &str = "SSH 서버 키 등록";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    target_id: String,
    host: String,
    port: u16,
    /// 입력한 지문이 없으면 선택한 공개 키 파일의 지문을 확인 창에 보여 준다.
    #[serde(default)]
    expected_fingerprint: Option<String>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Attempt {
    draft_id: String,
    draft_version: u32,
    attempt_id: String,
}

impl Attempt {
    fn valid(&self) -> bool {
        [&self.draft_id, &self.attempt_id].into_iter().all(|id| !id.is_empty() && id.len() <= 256)
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Rejection {
    InvalidTarget,
    InvalidEndpoint,
    EndpointMismatch,
    InvalidFingerprint,
    InvalidPublicKey,
    FingerprintMismatch,
    ExistingPinConflict,
    Cancelled,
    Busy,
    StaleDocument,
    StoreUnavailable,
}

impl Rejection {
    fn saved_message(self) -> &'static str {
        match self {
            Self::InvalidTarget => "저장된 SSH 연결 대상을 찾을 수 없습니다",
            Self::InvalidEndpoint => "SSH 접속 주소가 올바르지 않습니다",
            Self::EndpointMismatch => "저장된 SSH 연결 대상이 바뀌었습니다",
            Self::InvalidFingerprint => "SSH 호스트 키 지문이 올바르지 않습니다",
            Self::InvalidPublicKey => "SSH 서버 공개 키가 올바르지 않습니다",
            Self::FingerprintMismatch => "선택한 SSH 호스트 키의 지문이 다릅니다",
            Self::ExistingPinConflict => "다른 SSH 호스트 키가 이미 등록되어 있습니다",
            Self::Cancelled | Self::StaleDocument => "SSH 호스트 키 등록이 취소되었습니다",
            Self::Busy => "열려 있는 SSH 호스트 키 창을 먼저 닫으세요",
            Self::StoreUnavailable => "SSH 신뢰 저장소를 사용할 수 없습니다",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct VerifiedHost {
    host: String,
    port: u16,
    fingerprint: String,
    target_id: String,
}

#[derive(Serialize)]
struct Confirmation {
    ticket: String,
    #[serde(flatten)]
    host: VerifiedHost,
}

struct Reservation {
    document: u64,
    attempt: Option<Attempt>,
    ticket: String,
    cancelled: bool,
    ready: Option<Prepared>,
}

pub(crate) struct Registration {
    store: TrustStore,
    // ACL과 IPC 진입점에서 기본 웹뷰 한 곳만 허용한다.
    pending: Mutex<Option<Reservation>>,
}

impl Registration {
    pub(crate) fn new(store: TrustStore) -> Self {
        Self { store, pending: Mutex::default() }
    }

    // 앱은 페이지 로드 시작과 웹뷰 종료 양쪽에서 호출한다.
    pub(crate) fn document_changed(&self, owner: &str) {
        if owner == "main" && let Ok(mut pending) = self.pending.lock() {
            Self::invalidate(&mut pending);
        }
    }

    fn invalidate(pending: &mut Option<Reservation>) {
        if pending.as_ref().is_some_and(|slot| slot.ready.is_some()) {
            pending.take();
        } else if let Some(slot) = pending.as_mut() {
            slot.cancelled = true;
        }
    }

    fn begin(&self, document: u64, attempt: Option<Attempt>) -> Result<String, Rejection> {
        let mut pending = self.pending.lock().map_err(|_| Rejection::StoreUnavailable)?;
        if pending.is_some() {
            return Err(Rejection::Busy);
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| Rejection::StoreUnavailable)?;
        let ticket: String = random.into_iter().map(|byte| format!("{byte:02x}")).collect();
        *pending = Some(Reservation { document, attempt, ticket: ticket.clone(), cancelled: false, ready: None });
        Ok(ticket)
    }

    fn cancel(&self, document: u64, attempt: &Attempt) -> Result<(), Rejection> {
        let mut pending = self.pending.lock().map_err(|_| Rejection::StoreUnavailable)?;
        if pending.as_ref().is_some_and(|slot| slot.document == document && slot.attempt.as_ref() == Some(attempt)) {
            Self::invalidate(&mut pending);
        }
        Ok(())
    }

    fn commit(&self, document: u64, attempt: &Attempt, ticket: &str, sessions: &Sessions) -> Result<VerifiedHost, Rejection> {
        let mut pending = self.pending.lock().map_err(|_| Rejection::StoreUnavailable)?;
        let slot = pending.as_ref().filter(|slot| {
            !slot.cancelled && slot.document == document && slot.attempt.as_ref() == Some(attempt) && slot.ticket == ticket && slot.ready.is_some()
        }).ok_or(Rejection::Cancelled)?;
        // 문서 세대와 예약을 모두 잠근 채 일회 예약을 소비하고 핀을 저장한다.
        sessions.with_probes("main", slot.document, |_| {
            let prepared = pending.take().and_then(|slot| slot.ready).ok_or("SSH 호스트 키 예약 없음")?;
            Ok(prepared.commit(&self.store))
        }).map_err(|_| Rejection::StaleDocument)?
    }

    fn register_saved(
        &self,
        directory: &Path,
        target_id: String,
        sessions: &Sessions,
        document: u64,
        pick: impl FnOnce() -> Result<Option<PathBuf>, Rejection>,
        confirm: impl FnOnce(&Endpoint, &Fingerprint) -> Result<bool, Rejection>,
    ) -> Result<(), Rejection> {
        let ticket = self.begin(document, None)?;
        let reservation = Pending { state: self, ticket: &ticket };
        sessions.with_probes("main", document, |_| Ok(())).map_err(|_| Rejection::StaleDocument)?;
        let saved = crate::connections::with_saved_ssh(directory, &target_id, |saved| saved)
            .map_err(|_| Rejection::StoreUnavailable)?.ok_or(Rejection::InvalidTarget)?;
        let ssh = &saved.1;
        let prepared = reservation.verify(Request {
            target_id, host: ssh.host.clone(), port: ssh.port,
            expected_fingerprint: ssh.host_key_fingerprint.clone(),
        }, |_| Endpoint::new(&ssh.host, ssh.port).ok(), pick, confirm)?;

        // 프로필 → 예약 → 문서 순으로 잠그고 핀 기록까지 유지한다.
        let target_id = prepared.host.target_id.clone();
        crate::connections::with_saved_ssh(directory, &target_id, |current| {
            if current.as_ref() != Some(&saved) {
                return Err(Rejection::EndpointMismatch);
            }
            let _pending = reservation.lock_current()?;
            sessions.with_probes("main", document, |_| Ok(prepared.commit(&self.store).map(drop)))
                .map_err(|_| Rejection::StaleDocument)?
        }).map_err(|_| Rejection::StoreUnavailable)?
    }

    /// 메뉴 "호스트 키 확인…"이다. 열려 있는 확인 창이 있으면 새 창을 띄우지 않는다.
    fn trust_saved(
        &self,
        directory: &Path,
        target_id: &str,
        sessions: &Sessions,
        document: u64,
        check: impl FnOnce(&crate::connections::profile::Ssh) -> Result<(String, Checked), String>,
    ) -> Result<Trusted, String> {
        let ticket = self.begin(document, None).map_err(|error| error.saved_message())?;
        let _reservation = Pending { state: self, ticket: &ticket };
        sessions.with_probes("main", document, |_| Ok(())).map_err(|_| Rejection::StaleDocument.saved_message())?;
        let (_, ssh) = crate::connections::with_saved_ssh(directory, target_id, |saved| saved)
            .map_err(|_| Rejection::StoreUnavailable.saved_message())?
            .ok_or(Rejection::InvalidTarget.saved_message())?;
        let (fingerprint, checked) = check(&ssh)?;
        let status = match checked {
            Checked::Registered => "registered",
            Checked::Unchanged => "unchanged",
            Checked::Replaced => "replaced",
        };
        Ok(Trusted { fingerprint, status })
    }
}

/// `trust_saved_ssh_host` 응답이다. `status`는 `registered`, `unchanged`, `replaced` 중 하나다.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Trusted {
    fingerprint: String,
    status: &'static str,
}

/// 첫 연결 확인 창이다. 단추는 신뢰와 취소이고 기본 단추는 취소다.
pub(crate) fn host_key_trust(window: tauri::WebviewWindow) -> Confirm {
    Arc::new(move |description| dialogs::decide(&window, host_trust::TRUST_TITLE, description, "신뢰", false)
        .map_err(|_| "SSH 서버 키 확인 창을 열 수 없습니다".into()))
}

fn host_key_replace(window: tauri::WebviewWindow) -> Confirm {
    Arc::new(move |description| dialogs::decide(&window, host_trust::REPLACE_TITLE, description, "교체", true)
        .map_err(|_| "SSH 서버 키 변경 창을 열 수 없습니다".into()))
}

/// 저장된 SSH 세션의 서버 키를 다시 받아 등록하거나, 이전·새 지문을 보여 준 뒤 교체한다.
/// SSM 경유면 앱이 연 포트 전달로 키를 받고 바로 닫는다.
pub(crate) fn trust_saved(caller: &Webview, document: u64, target_id: String) -> Result<Trusted, String> {
    use crate::{connections::profile::SshTransport, tunnels::Transport};
    let config = caller.path().app_config_dir().map_err(|error| format!("앱 설정 디렉터리: {error}"))?;
    let state = caller.try_state::<Registration>().ok_or(Rejection::StoreUnavailable.saved_message())?;
    let window = caller.app_handle().get_webview_window(caller.label()).ok_or(Rejection::StoreUnavailable.saved_message())?;
    let (trust, replace) = (host_key_trust(window.clone()), host_key_replace(window));
    state.trust_saved(&config, &target_id, &caller.state::<Sessions>(), document, |ssh| match &ssh.transport {
        SshTransport::Direct {} => host_trust::check(&config, ssh, Via::Direct, &host_trust::keyscan, &trust, &replace),
        SshTransport::Ssm(target) => {
            let cancel = tokio_util::sync::CancellationToken::new();
            let (port, mut forward) = crate::tunnels::ssm::open_ssh_forward(target, ssh.port, &cancel)?;
            let result = host_trust::check(&config, ssh, Via::Forward(port), &host_trust::keyscan, &trust, &replace);
            let _ = forward.close();
            result
        }
    })
}

pub(crate) fn register_saved(caller: &Webview, document: u64, target_id: String) -> Result<(), String> {
    let config = caller.path().app_config_dir().map_err(|error| format!("앱 설정 디렉터리: {error}"))?;
    let state = caller.try_state::<Registration>().ok_or(Rejection::StoreUnavailable.saved_message())?;
    let window = caller.window();
    state.register_saved(&config, target_id, &caller.state::<Sessions>(), document,
        || dialogs::pick(&window),
        |endpoint, fingerprint| dialogs::confirm(&window, HOST_CONFIRM_TITLE, confirmation_message(endpoint, fingerprint)),
    ).map_err(|error| error.saved_message().into())
}

pub(crate) fn confirm_tunnel(window: &tauri::Window, description: String) -> Result<bool, String> {
    dialogs::confirm(window, "SSH 터널 승인", description)
        .map_err(|_| "SSH 터널 승인 창을 열 수 없습니다".into())
}

pub(crate) fn confirm_remote_editor(window: &tauri::Window, description: String) -> Result<bool, String> {
    dialogs::confirm(window, "SSH nano 연동 승인", description)
        .map_err(|_| "SSH nano 연동 승인 창을 열 수 없습니다".into())
}

pub(crate) fn confirm_stored_auth(window: &tauri::Window, description: String) -> Result<bool, String> {
    dialogs::confirm(window, "SSH 저장 암호 승인", description)
        .map_err(|_| "SSH 저장 암호 승인 창을 열 수 없습니다".into())
}

#[cfg(all(feature = "stored-auth", any(windows, target_os = "linux")))]
pub(crate) fn confirm_elevation(window: &tauri::Window, description: String) -> Result<bool, String> {
    dialogs::confirm(window, "sudo 승격 승인", description)
        .map_err(|_| "sudo 승격 승인 창을 열 수 없습니다".into())
}

fn confirmation_message(endpoint: &Endpoint, fingerprint: &Fingerprint) -> String {
    format!("서버: {}\n포트: {}\nSHA256 지문: {fingerprint}", endpoint.host, endpoint.port)
}

pub(crate) fn dispatch(invoke: Invoke) {
    let caller = invoke.message.webview_ref().clone();
    if caller.label() != "main" || caller.window_ref().label() != "main" || invoke.acl.is_none() {
        invoke.resolver.reject("앱의 로컬 기본 창에서만 SSH 호스트 키를 등록할 수 있습니다");
        return;
    }
    let Some(state) = caller.try_state::<Registration>() else {
        invoke.resolver.invoke_error(Rejection::StoreUnavailable.into());
        return;
    };
    let document = caller.state::<Sessions>().document(caller.label());
    let name = match invoke.message.command() {
        COMMIT => COMMIT,
        CANCEL => CANCEL,
        _ => COMMAND,
    };
    let attempt = match Attempt::from_command(CommandItem {
        name, key: "attempt", plugin: None, message: &invoke.message, acl: &invoke.acl,
    }) {
        Ok(attempt) if attempt.valid() => attempt,
        Ok(_) => { invoke.resolver.invoke_error(Rejection::InvalidTarget.into()); return; }
        Err(error) => { invoke.resolver.invoke_error(error); return; }
    };
    if name == CANCEL {
        match state.cancel(document, &attempt) {
            Ok(()) => invoke.resolver.resolve(()),
            Err(error) => invoke.resolver.invoke_error(error.into()),
        }
        return;
    }
    if name == COMMIT {
        let ticket = match String::from_command(CommandItem {
            name, key: "ticket", plugin: None, message: &invoke.message, acl: &invoke.acl,
        }) {
            Ok(ticket) => ticket,
            Err(error) => { invoke.resolver.invoke_error(error); return; }
        };
        let caller = caller.clone();
        invoke.resolver.respond_async(async move {
            tauri::async_runtime::spawn_blocking(move || caller.state::<Registration>()
                .commit(document, &attempt, &ticket, &caller.state::<Sessions>()))
                .await.map_err(|_| Rejection::StoreUnavailable)?.map_err(Into::into)
        });
        return;
    }
    let webview = caller.clone();
    let Invoke { message, resolver, acl } = invoke;
    let request = match Request::from_command(CommandItem {
        name: COMMAND, key: "request", plugin: None, message: &message, acl: &acl,
    }) {
        Ok(request) => request,
        Err(error) => { resolver.invoke_error(error); return; }
    };
    let draft = match Draft::from_command(CommandItem {
        name: COMMAND, key: "draft", plugin: None, message: &message, acl: &acl,
    }) {
        Ok(draft) => match draft.validate() {
            Ok(validated) => validated,
            Err(_) => { resolver.invoke_error(Rejection::InvalidTarget.into()); return; }
        },
        Err(error) => { resolver.invoke_error(error); return; }
    };
    let ticket = match state.begin(document, Some(attempt)) {
        Ok(ticket) => ticket,
        Err(error) => { resolver.invoke_error(error.into()); return; }
    };
    resolver.respond_async(async move {
        tauri::async_runtime::spawn_blocking(move || {
            let state = webview.state::<Registration>();
            let reservation = Pending { state: &state, ticket: &ticket };
            let sessions = webview.state::<Sessions>();
            sessions.with_probes("main", document, |_| Ok(())).map_err(|_| Rejection::StaleDocument)?;
            let window = webview.window();
            let expected_fingerprint = request.expected_fingerprint.clone();
            let prepared = reservation.verify(
                request,
                |id| draft.ssh_pin_endpoint(id, expected_fingerprint.as_deref()),
                || dialogs::pick(&window),
                |endpoint, fingerprint| dialogs::confirm(&window, HOST_CONFIRM_TITLE, confirmation_message(endpoint, fingerprint)),
            )?;
            let host = prepared.host.clone();
            reservation.ready(prepared)?;
            Ok::<_, Rejection>(Confirmation { ticket, host })
        }).await.map_err(|_| Rejection::StoreUnavailable)?.map_err(Into::into)
    });
}

struct Prepared {
    host: VerifiedHost,
    endpoint: Endpoint,
    key: String,
}

impl Prepared {
    fn commit(self, store: &TrustStore) -> Result<VerifiedHost, Rejection> {
        let approved = ApprovedHost::confirm(&self.endpoint, &self.endpoint, &self.host.fingerprint)
            .map_err(registration_error)?;
        store.register(&approved, &self.key).map_err(registration_error)?;
        Ok(self.host)
    }
}

struct Pending<'a> {
    state: &'a Registration,
    ticket: &'a str,
}

impl Pending<'_> {
    fn verify(
        &self,
        request: Request,
        resolve: impl Fn(&str) -> Option<Endpoint>,
        pick: impl FnOnce() -> Result<Option<PathBuf>, Rejection>,
        confirm: impl FnOnce(&Endpoint, &Fingerprint) -> Result<bool, Rejection>,
    ) -> Result<Prepared, Rejection> {
        self.current()?;
        if request.target_id.is_empty() || request.target_id.len() > 256 {
            return Err(Rejection::InvalidTarget);
        }
        let endpoint = resolve(&request.target_id).ok_or(Rejection::InvalidTarget)?;
        let shown = Endpoint::new(&request.host, request.port).map_err(|_| Rejection::InvalidEndpoint)?;
        match &request.expected_fingerprint {
            Some(expected) if expected.len() != 50 => return Err(Rejection::InvalidFingerprint),
            Some(expected) => { ApprovedHost::confirm(&endpoint, &shown, expected).map_err(registration_error)?; }
            None if endpoint != shown => return Err(Rejection::EndpointMismatch),
            None => {}
        }
        let path = pick()?.ok_or(Rejection::Cancelled)?;
        self.current()?;
        let key = read_public_key(&path)?;
        let fingerprint = plain_public_key(&key).map_err(registration_error)?.fingerprint(HashAlg::Sha256);
        if request.expected_fingerprint.as_ref().is_some_and(|expected| *expected != fingerprint.to_string()) {
            return Err(Rejection::FingerprintMismatch);
        }
        self.current()?;
        if !confirm(&endpoint, &fingerprint)? {
            return Err(Rejection::Cancelled);
        }
        self.current()?;
        if resolve(&request.target_id).as_ref() != Some(&endpoint) {
            return Err(Rejection::EndpointMismatch);
        }
        let host = VerifiedHost { host: request.host, port: endpoint.port, fingerprint: fingerprint.to_string(), target_id: request.target_id };
        Ok(Prepared { host, endpoint, key })
    }

    fn ready(self, prepared: Prepared) -> Result<(), Rejection> {
        let mut pending = self.lock_current()?;
        pending.as_mut().ok_or(Rejection::Cancelled)?.ready = Some(prepared);
        Ok(())
    }

    fn lock_current(&self) -> Result<std::sync::MutexGuard<'_, Option<Reservation>>, Rejection> {
        let pending = self.state.pending.lock().map_err(|_| Rejection::StoreUnavailable)?;
        if !pending.as_ref().is_some_and(|slot| slot.ticket == self.ticket && !slot.cancelled) {
            return Err(Rejection::Cancelled);
        }
        Ok(pending)
    }

    fn current(&self) -> Result<(), Rejection> {
        self.lock_current().map(drop)
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.state.pending.lock()
            && pending.as_ref().is_some_and(|slot| slot.ticket == self.ticket && slot.ready.is_none())
        {
            pending.take();
        }
    }
}

fn read_public_key(path: &Path) -> Result<String, Rejection> {
    if !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("pub")) {
        return Err(Rejection::InvalidPublicKey);
    }
    super::public_key_file::read(path).map_err(|_| Rejection::InvalidPublicKey)
}

fn registration_error(error: Error) -> Rejection {
    match error {
        Error::ApprovedEndpointMismatch => Rejection::EndpointMismatch,
        Error::InvalidFingerprint => Rejection::InvalidFingerprint,
        Error::InvalidPublicKey => Rejection::InvalidPublicKey,
        Error::FingerprintMismatch => Rejection::FingerprintMismatch,
        Error::ExistingPinConflict => Rejection::ExistingPinConflict,
        _ => Rejection::StoreUnavailable,
    }
}

#[cfg(test)]
mod tests;
