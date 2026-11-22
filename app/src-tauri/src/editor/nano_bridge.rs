use std::{collections::HashMap, sync::{Arc, Mutex}, time::Duration};

use serde::Serialize;
use tauri::{AppHandle, Emitter, EventTarget, Manager};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::terminal::Sessions;

use super::{
    cli_open::MAIN,
    nano_wire::{Request, Response},
    sftp_ipc::{BridgeSlot, Opened, SftpIpc, SharedSftpNamespace},
};

const OFFER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(crate) struct NanoBridge(Arc<Mutex<HashMap<String, Entry>>>);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NanoOffer {
    offer_id: String,
    session_id: String,
}

#[derive(Serialize)]
pub(crate) struct NanoClaim {
    handle: String,
    name: String,
    text: String,
}

struct Entry {
    document: u64,
    session_id: String,
    handle: String,
    request: CancellationToken,
    slot: CancellationToken,
    phase: Phase,
    done: oneshot::Sender<Outcome>,
}

enum Phase {
    Pending(NanoClaim),
    Claimed,
    Mounted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Unattached,
    Failed,
    Closed,
}

impl Entry {
    fn accepts(&self, document: u64) -> bool {
        self.document == document && !self.request.is_cancelled() && !self.slot.is_cancelled()
    }

    fn finish(self, outcome: Outcome) {
        let _ = self.done.send(outcome);
    }
}

impl NanoBridge {
    pub(crate) fn pending(&self, label: &str, document: u64) -> Vec<NanoOffer> {
        if label != MAIN {
            return Vec::new();
        }
        self.0.lock().unwrap().iter().filter_map(|(id, entry)| {
            (entry.accepts(document) && matches!(entry.phase, Phase::Pending(_))).then(|| NanoOffer {
                offer_id: id.clone(),
                session_id: entry.session_id.clone(),
            })
        }).collect()
    }

    pub(crate) fn claim(&self, label: &str, document: u64, offer_id: &str) -> Option<NanoClaim> {
        if label != MAIN {
            return None;
        }
        let mut entries = self.0.lock().unwrap();
        let entry = entries.get_mut(offer_id)?;
        if !entry.accepts(document) || !matches!(entry.phase, Phase::Pending(_)) {
            return None;
        }
        match std::mem::replace(&mut entry.phase, Phase::Claimed) {
            Phase::Pending(claim) => Some(claim),
            _ => unreachable!(),
        }
    }

    pub(crate) fn mounted(&self, label: &str, document: u64, offer_id: &str, handle: &str) -> bool {
        if label != MAIN {
            return false;
        }
        let mut entries = self.0.lock().unwrap();
        let Some(entry) = entries.get_mut(offer_id) else { return false; };
        if !entry.accepts(document) || !matches!(entry.phase, Phase::Claimed) || entry.handle != handle {
            return false;
        }
        entry.phase = Phase::Mounted;
        true
    }

    pub(crate) fn claim_delivery_failed(&self, label: &str, document: u64, offer_id: &str) -> bool {
        if label != MAIN {
            return false;
        }
        let mut entries = self.0.lock().unwrap();
        if !entries.get(offer_id).is_some_and(|entry| {
            entry.document == document && matches!(entry.phase, Phase::Claimed)
        }) {
            return false;
        }
        entries.remove(offer_id).unwrap().finish(Outcome::Failed);
        true
    }

    fn offer(
        &self, offer_id: String, document: u64, session_id: &str, opened: Opened,
        request: CancellationToken, slot: CancellationToken,
    ) -> (NanoOffer, oneshot::Receiver<Outcome>) {
        let (done, receive) = oneshot::channel();
        let Opened { handle, name, text } = opened;
        self.0.lock().unwrap().insert(offer_id.clone(), Entry {
            document,
            session_id: session_id.into(),
            handle: handle.clone(),
            request,
            slot,
            phase: Phase::Pending(NanoClaim { handle, name, text }),
            done,
        });
        (NanoOffer { offer_id, session_id: session_id.into() }, receive)
    }

    fn interrupted(&self, offer_id: &str, mount_timeout: bool) {
        let mut entries = self.0.lock().unwrap();
        let Some(entry) = entries.get(offer_id) else { return; };
        let outcome = match entry.phase {
            Phase::Pending(_) => Outcome::Unattached,
            Phase::Mounted if mount_timeout => return,
            Phase::Claimed | Phase::Mounted => Outcome::Failed,
        };
        entries.remove(offer_id).unwrap().finish(outcome);
    }

    fn file_closed(entries: &Mutex<HashMap<String, Entry>>, offer_id: &str) {
        if let Some(entry) = entries.lock().unwrap().remove(offer_id) {
            let outcome = match entry.phase {
                Phase::Pending(_) => Outcome::Unattached,
                Phase::Mounted if entry.accepts(entry.document) => Outcome::Closed,
                Phase::Claimed | Phase::Mounted => Outcome::Failed,
            };
            entry.finish(outcome);
        }
    }
}

async fn open(
    app: &AppHandle, slot: &BridgeSlot, document: u64, parent: &str, path: &[u8],
    request: &CancellationToken,
) -> Outcome {
    let Ok(path) = std::str::from_utf8(path) else { return Outcome::Unattached; };
    let sftp = app.state::<SftpIpc>();
    let sessions = app.state::<Sessions>();
    let slot_cancel = slot.cancellation();
    if request.is_cancelled() || slot_cancel.is_cancelled() || sessions.document(MAIN) != document {
        return Outcome::Unattached;
    }
    let Ok(opened) = sftp.open_bridge(slot, &sessions, path).await else { return Outcome::Unattached; };
    let handle = opened.handle.clone();
    let offer_id = uuid::Uuid::new_v4().simple().to_string();
    let state = app.state::<NanoBridge>();
    let callback_state = Arc::downgrade(&state.0);
    let callback_id = offer_id.clone();
    // 감시를 먼저 설치해 마운트 승인 직후의 닫기를 놓치지 않는다.
    if sftp.watch_bridge_close(slot, &handle, move || {
        if let Some(entries) = callback_state.upgrade() {
            NanoBridge::file_closed(&entries, &callback_id);
        }
    }).await.is_err() || request.is_cancelled() || slot_cancel.is_cancelled() {
        sftp.discard_bridge_file(slot, &handle).await;
        return Outcome::Unattached;
    }
    let (offer, mut done) = state.offer(offer_id.clone(), document, parent, opened, request.clone(), slot_cancel.clone());
    if app.emit_to(EventTarget::webview(MAIN), "editor-remote-nano-open", offer).is_err() {
        state.interrupted(&offer_id, false);
    }
    let deadline = tokio::time::sleep(OFFER_TIMEOUT);
    tokio::pin!(deadline);
    let mut timed_out = false;
    let outcome = loop {
        tokio::select! {
            biased;
            result = &mut done => break result.unwrap_or(Outcome::Failed),
            _ = request.cancelled() => {
                state.interrupted(&offer_id, false);
                break done.await.unwrap_or(Outcome::Failed);
            }
            _ = slot_cancel.cancelled() => {
                state.interrupted(&offer_id, false);
                break done.await.unwrap_or(Outcome::Failed);
            }
            _ = &mut deadline, if !timed_out => {
                state.interrupted(&offer_id, true);
                timed_out = true;
            }
        }
    };
    if outcome == Outcome::Unattached {
        sftp.discard_bridge_file(slot, &handle).await;
    }
    outcome
}

async fn drive_open(
    id: u64, opened: impl Future<Output = Outcome>, request: &CancellationToken,
    cancel: &CancellationToken, slot_cancel: &CancellationToken, requests: &mut mpsc::Receiver<Request>,
) -> Option<Outcome> {
    tokio::pin!(opened);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            outcome = &mut opened => return Some(outcome),
            message = requests.recv() => match message {
                Some(Request::Cancel { id: cancelled }) if cancelled == id => request.cancel(),
                Some(Request::Cancel { .. }) => {},
                None | Some(Request::Open { .. }) => break,
            },
        }
    }
    request.cancel();
    slot_cancel.cancel();
    opened.await;
    None
}

// 배치 공개키 인증과 관리자가 확인한 셸·SFTP 공통 경로를 선택한 탭에서만 호출한다.
pub(crate) async fn run(
    app: AppHandle, owner: String, document: u64, parent: String, cancel: CancellationToken,
    mut requests: mpsc::Receiver<Request>, responses: mpsc::Sender<Response>,
) {
    let sftp = app.state::<SftpIpc>();
    let slot = (owner == MAIN && app.get_webview_window(MAIN).is_some_and(|webview| webview.as_ref().window().label() == MAIN))
        .then(|| sftp.bind_bridge(&owner, document, &parent, SharedSftpNamespace::AdministratorConfirmed).ok())
        .flatten();
    loop {
        let message = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            message = requests.recv() => message,
        };
        let (id, path) = match message {
            Some(Request::Open { id, path }) => (id, path),
            Some(Request::Cancel { .. }) => continue,
            None => return,
        };
        let outcome = if let Some(slot) = &slot {
            let request = cancel.child_token();
            let slot_cancel = slot.cancellation();
            let opened = open(&app, slot, document, &parent, &path, &request);
            let Some(outcome) = drive_open(id, opened, &request, &cancel, &slot_cancel, &mut requests).await else {
                return;
            };
            outcome
        } else {
            Outcome::Unattached
        };
        let response = match outcome {
            Outcome::Closed => Response::Closed { id },
            Outcome::Unattached => Response::Unattached { id },
            Outcome::Failed => Response::Failed { id },
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            result = responses.send(response) => if result.is_err() { return; },
        }
    }
}

#[cfg(test)]
mod tests;
