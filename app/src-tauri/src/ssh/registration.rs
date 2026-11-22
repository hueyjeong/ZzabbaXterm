use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};

use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult};
use serde::{Deserialize, Serialize};
use ssh_key::{Fingerprint, HashAlg};
use tauri::{Manager, ipc::{CommandArg, CommandItem, Invoke}};
use crate::{connections::profile::Draft, terminal::Sessions};

use super::{ApprovedHost, Endpoint, Error, TrustStore, trust::plain_public_key};

pub(crate) const COMMAND: &str = "verify_ssh_host";
const MAX_KEY_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    target_id: String,
    host: String,
    port: u16,
    expected_fingerprint: String,
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

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct VerifiedHost {
    host: String,
    port: u16,
    fingerprint: String,
    target_id: String,
}

pub(crate) struct Registration {
    store: TrustStore,
    pending: Mutex<HashMap<String, bool>>,
}

impl Registration {
    pub(crate) fn new(store: TrustStore) -> Self {
        Self { store, pending: Mutex::default() }
    }

    // 앱은 페이지 로드 시작과 웹뷰 종료 양쪽에서 호출한다.
    pub(crate) fn document_changed(&self, owner: &str) {
        if let Ok(mut pending) = self.pending.lock()
            && let Some(current) = pending.get_mut(owner)
        {
            *current = false;
        }
    }

    fn begin(&self, owner: &str) -> Result<(), Rejection> {
        use std::collections::hash_map::Entry;
        let mut pending = self.pending.lock().map_err(|_| Rejection::StoreUnavailable)?;
        match pending.entry(owner.into()) {
            Entry::Occupied(_) => Err(Rejection::Busy),
            Entry::Vacant(entry) => {
                entry.insert(true);
                Ok(())
            }
        }
    }
}

/// 동기 invoke에서 로컬 기본 창의 문서와 Rust가 검증한 초안을 예약한다.
pub(crate) fn verify_ssh_host(invoke: Invoke) {
    let Invoke { message, resolver, acl } = invoke;
    let webview = message.webview_ref().clone();
    if webview.label() != "main" || webview.window_ref().label() != "main" || acl.is_none() {
        resolver.reject("앱의 로컬 기본 창에서만 SSH 호스트 키를 등록할 수 있습니다");
        return;
    }
    let document = webview.state::<Sessions>().document(webview.label());
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
    let Some(state) = webview.try_state::<Registration>() else {
        resolver.invoke_error(Rejection::StoreUnavailable.into());
        return;
    };
    // 비동기 작업을 예약하기 전에 발신 문서의 요청을 예약한다.
    if let Err(error) = state.begin(webview.label()) {
        resolver.invoke_error(error.into());
        return;
    }
    resolver.respond_async(async move {
        tauri::async_runtime::spawn_blocking(move || {
            let state = webview.state::<Registration>();
            let window = webview.window();
            let sessions = webview.state::<Sessions>();
            let current = || sessions.with_probes(webview.label(), document, |_| Ok(())).is_ok();
            let expected_fingerprint = request.expected_fingerprint.clone();
            let resolve = |id: &str| {
                current().then(|| draft.ssh_pin_endpoint(id, &expected_fingerprint)).flatten()
            };
            Pending { state: &state, owner: webview.label() }.verify(
                request,
                resolve,
                || FileDialog::new()
                    .set_parent(&window)
                    .set_title("SSH 서버 공개 키 선택")
                    .add_filter("OpenSSH 공개 키", &["pub"])
                    .pick_file(),
                |endpoint, fingerprint| MessageDialog::new()
                    .set_parent(&window)
                    .set_title("SSH 서버 키 등록")
                    .set_description(format!("서버: {}\n포트: {}\nSHA256 지문: {fingerprint}", endpoint.host, endpoint.port))
                    .set_buttons(MessageButtons::OkCancel)
                    .show() == MessageDialogResult::Ok,
            )
        }).await.map_err(|_| Rejection::StoreUnavailable)?.map_err(Into::into)
    });
}

struct Pending<'a> {
    state: &'a Registration,
    owner: &'a str,
}

impl Pending<'_> {
    fn verify(
        self,
        request: Request,
        resolve: impl Fn(&str) -> Option<Endpoint>,
        pick: impl FnOnce() -> Option<PathBuf>,
        confirm: impl FnOnce(&Endpoint, &Fingerprint) -> bool,
    ) -> Result<VerifiedHost, Rejection> {
        self.current()?;
        if request.target_id.is_empty() || request.target_id.len() > 256 {
            return Err(Rejection::InvalidTarget);
        }
        let endpoint = resolve(&request.target_id).ok_or(Rejection::InvalidTarget)?;
        let shown = Endpoint::new(&request.host, request.port).map_err(|_| Rejection::InvalidEndpoint)?;
        if request.expected_fingerprint.len() != 50 {
            return Err(Rejection::InvalidFingerprint);
        }
        let approved = ApprovedHost::confirm(&endpoint, &shown, &request.expected_fingerprint)
            .map_err(registration_error)?;
        let path = pick().ok_or(Rejection::Cancelled)?;
        self.current()?;
        let bytes = read_public_key(&path)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| Rejection::InvalidPublicKey)?;
        let fingerprint = plain_public_key(text).map_err(registration_error)?.fingerprint(HashAlg::Sha256);
        if fingerprint.to_string() != request.expected_fingerprint {
            return Err(Rejection::FingerprintMismatch);
        }
        self.current()?;
        if !confirm(&endpoint, &fingerprint) {
            return Err(Rejection::Cancelled);
        }
        // 문서 취소와 핀 저장의 순서를 같은 잠금에서 확정한다.
        let pending = self.lock_current()?;
        if resolve(&request.target_id).as_ref() != Some(&endpoint) {
            return Err(Rejection::EndpointMismatch);
        }
        self.state.store.register(&approved, text).map_err(registration_error)?;
        drop(pending);
        Ok(VerifiedHost { host: request.host, port: endpoint.port, fingerprint: request.expected_fingerprint, target_id: request.target_id })
    }

    fn lock_current(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, bool>>, Rejection> {
        let pending = self.state.pending.lock().map_err(|_| Rejection::StoreUnavailable)?;
        if pending.get(self.owner) != Some(&true) {
            return Err(Rejection::StaleDocument);
        }
        Ok(pending)
    }

    fn current(&self) -> Result<(), Rejection> {
        self.lock_current().map(drop)
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.state.pending.lock() {
            pending.remove(self.owner);
        }
    }
}

fn read_public_key(path: &Path) -> Result<Vec<u8>, Rejection> {
    use Rejection::InvalidPublicKey;
    if !path.is_absolute() || !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("pub")) {
        return Err(InvalidPublicKey);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    let _parents = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ};
        super::config_path(path).map_err(|_| InvalidPublicKey)?;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).share_mode(FILE_SHARE_READ);
        super::trust_windows::creation_parent(path).map_err(|_| InvalidPublicKey)?
    };
    let file = options.open(path).map_err(|_| InvalidPublicKey)?;
    let metadata = file.metadata().map_err(|_| InvalidPublicKey)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(InvalidPublicKey);
        }
    }
    if !metadata.is_file() || metadata.len() > MAX_KEY_BYTES as u64 {
        return Err(InvalidPublicKey);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_KEY_BYTES + 1) as u64).read_to_end(&mut bytes).map_err(|_| InvalidPublicKey)?;
    if bytes.len() > MAX_KEY_BYTES {
        return Err(InvalidPublicKey);
    }
    Ok(bytes)
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
