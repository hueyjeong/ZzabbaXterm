mod dialogs;

use std::{path::{Path, PathBuf}, sync::{Arc, Mutex}};

use serde::Serialize;
use ssh_key::{Fingerprint, HashAlg};
use tauri::{Manager, Webview};
use crate::terminal::Sessions;

use super::{ApprovedHost, Endpoint, Error, TrustStore, host_trust::{self, Checked, Confirm, Via}, trust::plain_public_key};

const HOST_CONFIRM_TITLE: &str = "SSH 서버 키 등록";

struct Request {
    target_id: String,
    host: String,
    port: u16,
    /// 입력한 지문이 없으면 선택한 공개 키 파일의 지문을 확인 창에 보여 준다.
    expected_fingerprint: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
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

struct Reservation {
    ticket: String,
    cancelled: bool,
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
        if owner == "main" && let Ok(mut pending) = self.pending.lock() && let Some(slot) = pending.as_mut() {
            slot.cancelled = true;
        }
    }

    fn begin(&self) -> Result<String, Rejection> {
        let mut pending = self.pending.lock().map_err(|_| Rejection::StoreUnavailable)?;
        if pending.is_some() {
            return Err(Rejection::Busy);
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| Rejection::StoreUnavailable)?;
        let ticket: String = random.into_iter().map(|byte| format!("{byte:02x}")).collect();
        *pending = Some(Reservation { ticket: ticket.clone(), cancelled: false });
        Ok(ticket)
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
        let ticket = self.begin()?;
        let reservation = Pending { state: self, ticket: &ticket };
        sessions.with_probes("main", document, |_| Ok(())).map_err(|_| Rejection::StaleDocument)?;
        let saved = crate::connections::with_saved_ssh(directory, &target_id, |saved| saved)
            .map_err(|_| Rejection::StoreUnavailable)?.ok_or(Rejection::InvalidTarget)?;
        let ssh = &saved.1;
        let prepared = reservation.verify(Request {
            target_id: target_id.clone(), host: ssh.host.clone(), port: ssh.port,
            expected_fingerprint: ssh.host_key_fingerprint.clone(),
        }, |_| Endpoint::new(&ssh.host, ssh.port).ok(), pick, confirm)?;

        // 프로필 → 예약 → 문서 순으로 잠그고 핀 기록까지 유지한다.
        crate::connections::with_saved_ssh(directory, &target_id, |current| {
            if current.as_ref() != Some(&saved) {
                return Err(Rejection::EndpointMismatch);
            }
            let _pending = reservation.lock_current()?;
            sessions.with_probes("main", document, |_| Ok(prepared.commit(&self.store)))
                .map_err(|_| Rejection::StaleDocument)?
        }).map_err(|_| Rejection::StoreUnavailable)?
    }

    /// 메뉴 "호스트 키 확인…"이다. 열려 있는 확인 창이 있으면 새 창을 띄우지 않는다.
    /// 확인 창을 승인한 직후 문서를 다시 확인해, 그사이 페이지가 바뀌었으면 핀을 쓰지 않는다.
    #[allow(clippy::too_many_arguments)]
    fn trust_saved<F: crate::tunnels::Transport + Send + 'static>(
        &self,
        directory: &Path,
        target_id: &str,
        sessions: &Sessions,
        document: u64,
        (trust, replace): (&Confirm, &Confirm),
        scan: host_trust::Scan<'_>,
        open: crate::ssh::terminal::OpenForward<F>,
    ) -> Result<Trusted, String> {
        use crate::connections::profile::SshTransport;
        let ticket = self.begin().map_err(|error| error.saved_message())?;
        let _reservation = Pending { state: self, ticket: &ticket };
        let current = || sessions.with_probes("main", document, |_| Ok(())).map_err(|_| Rejection::StaleDocument.saved_message().to_owned());
        current()?;
        let (_, ssh) = crate::connections::with_saved_ssh(directory, target_id, |saved| saved)
            .map_err(|_| Rejection::StoreUnavailable.saved_message())?
            .ok_or(Rejection::InvalidTarget.saved_message())?;
        let approved = |prompt: &Confirm, message: String, cancel: &tokio_util::sync::CancellationToken| -> Result<bool, String> {
            let accepted = prompt(message, cancel)?;
            if accepted { current()?; }
            Ok(accepted)
        };
        let trust = |message, cancel: &_| approved(trust, message, cancel);
        let replace = |message, cancel: &_| approved(replace, message, cancel);
        let cancel = tokio_util::sync::CancellationToken::new();
        let (fingerprint, checked) = match &ssh.transport {
            SshTransport::Direct {} => host_trust::check(directory, &ssh, Via::Direct, scan, &trust, &replace, &cancel)?,
            SshTransport::Ssm(target) => {
                let (port, mut forward) = open(target, ssh.port, &cancel)?;
                let result = host_trust::check(directory, &ssh, Via::Forward(port), scan, &trust, &replace, &cancel);
                let _ = forward.close();
                result?
            }
        };
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

/// 첫 연결 확인 창이다. 단추는 신뢰와 취소이고 기본 단추는 취소다. 토큰이 취소되거나 5분이 지나면 닫힌다.
pub(crate) fn host_key_trust(window: tauri::WebviewWindow) -> Confirm {
    Arc::new(move |description, cancel| dialogs::decide(&window, dialogs::Decision::Trust, description, cancel)
        .map_err(|_| "SSH 서버 키 확인 창을 열 수 없습니다".into()))
}

fn host_key_replace(window: tauri::WebviewWindow) -> Confirm {
    Arc::new(move |description, cancel| dialogs::decide(&window, dialogs::Decision::Replace, description, cancel)
        .map_err(|_| "SSH 서버 키 변경 창을 열 수 없습니다".into()))
}

/// 저장된 SSH 세션의 서버 키를 다시 받아 등록하거나, 이전·새 지문을 보여 준 뒤 교체한다.
/// SSM 경유면 앱이 연 포트 전달로 키를 받고 바로 닫는다.
pub(crate) fn trust_saved(caller: &Webview, document: u64, target_id: String) -> Result<Trusted, String> {
    let config = caller.path().app_config_dir().map_err(|error| format!("앱 설정 디렉터리: {error}"))?;
    let state = caller.try_state::<Registration>().ok_or(Rejection::StoreUnavailable.saved_message())?;
    let window = caller.app_handle().get_webview_window(caller.label()).ok_or(Rejection::StoreUnavailable.saved_message())?;
    let (trust, replace) = (host_key_trust(window.clone()), host_key_replace(window));
    state.trust_saved(&config, &target_id, &caller.state::<Sessions>(), document, (&trust, &replace),
        &host_trust::keyscan, crate::tunnels::ssm::open_ssh_forward)
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

struct Prepared {
    endpoint: Endpoint,
    fingerprint: String,
    key: String,
}

impl Prepared {
    fn commit(self, store: &TrustStore) -> Result<(), Rejection> {
        let approved = ApprovedHost::confirm(&self.endpoint, &self.endpoint, &self.fingerprint)
            .map_err(registration_error)?;
        store.register(&approved, &self.key).map_err(registration_error)
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
        Ok(Prepared { endpoint, fingerprint: fingerprint.to_string(), key })
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
            && pending.as_ref().is_some_and(|slot| slot.ticket == self.ticket)
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
