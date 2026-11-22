mod dialogs;

use std::{path::{Path, PathBuf}, sync::Mutex};

use serde::{Deserialize, Serialize};
use ssh_key::{Fingerprint, HashAlg};
use tauri::{Manager, ipc::{CommandArg, CommandItem, Invoke}};
use crate::{connections::profile::Draft, terminal::Sessions};

use super::{ApprovedHost, Endpoint, Error, TrustStore, trust::plain_public_key};

pub(crate) const COMMAND: &str = "verify_ssh_host";
pub(crate) const COMMIT: &str = "commit_ssh_host";
pub(crate) const CANCEL: &str = "cancel_ssh_host";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    target_id: String,
    host: String,
    port: u16,
    expected_fingerprint: String,
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
    attempt: Attempt,
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

    fn begin(&self, document: u64, attempt: Attempt) -> Result<String, Rejection> {
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
        if pending.as_ref().is_some_and(|slot| slot.document == document && &slot.attempt == attempt) {
            Self::invalidate(&mut pending);
        }
        Ok(())
    }

    fn commit(&self, document: u64, attempt: &Attempt, ticket: &str, sessions: &Sessions) -> Result<VerifiedHost, Rejection> {
        let mut pending = self.pending.lock().map_err(|_| Rejection::StoreUnavailable)?;
        let slot = pending.as_ref().filter(|slot| {
            !slot.cancelled && slot.document == document && &slot.attempt == attempt && slot.ticket == ticket && slot.ready.is_some()
        }).ok_or(Rejection::Cancelled)?;
        // 문서 세대와 예약을 모두 잠근 채 일회 예약을 소비하고 핀을 저장한다.
        sessions.with_probes("main", slot.document, |_| {
            let prepared = pending.take().and_then(|slot| slot.ready).ok_or("SSH 호스트 키 예약 없음")?;
            Ok(prepared.commit(&self.store))
        }).map_err(|_| Rejection::StaleDocument)?
    }
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
    let ticket = match state.begin(document, attempt) {
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
                |id| draft.ssh_pin_endpoint(id, &expected_fingerprint),
                || dialogs::pick(&window),
                |endpoint, fingerprint| dialogs::confirm(&window, format!(
                    "서버: {}\n포트: {}\nSHA256 지문: {fingerprint}", endpoint.host, endpoint.port,
                )),
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
        if request.expected_fingerprint.len() != 50 {
            return Err(Rejection::InvalidFingerprint);
        }
        ApprovedHost::confirm(&endpoint, &shown, &request.expected_fingerprint).map_err(registration_error)?;
        let path = pick()?.ok_or(Rejection::Cancelled)?;
        self.current()?;
        let key = read_public_key(&path)?;
        let fingerprint = plain_public_key(&key).map_err(registration_error)?.fingerprint(HashAlg::Sha256);
        if fingerprint.to_string() != request.expected_fingerprint {
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
        let host = VerifiedHost { host: request.host, port: endpoint.port, fingerprint: request.expected_fingerprint, target_id: request.target_id };
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
