use std::{collections::HashMap, sync::Mutex, time::Duration};

use serde::Serialize;
use tauri::{AppHandle, Emitter, EventTarget, Manager};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::terminal::Sessions;

use super::{
    cli_open::MAIN,
    nano_wire::{Response, publication::ActiveOpen},
    sftp_ipc::{BridgeSlot, Opened, SftpIpc, SharedSftpNamespace},
};

mod publication;
pub(crate) use publication::{PUBLICATION_IO_LIMIT, PublicationOperation, PublicationReply, PublicationWork, PublishError, Publisher, transfer_allowance};

const OFFER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(crate) struct NanoBridge(Mutex<HashMap<String, Entry>>);

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum MountStatus {
    Mounted,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
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

    pub(super) fn mount_failed(
        &self, label: &str, document: u64, offer_id: &str, handle: Option<&str>,
    ) -> MountStatus {
        if label != MAIN {
            return MountStatus::Unavailable;
        }
        let mut entries = self.0.lock().unwrap();
        let Some(entry) = entries.get(offer_id) else { return MountStatus::Unavailable; };
        if entry.document != document || handle.is_some_and(|handle| entry.handle != handle) {
            return MountStatus::Unavailable;
        }
        let outcome = match entry.phase {
            Phase::Mounted if handle.is_none() => return MountStatus::Unavailable,
            Phase::Mounted if entry.accepts(document) => return MountStatus::Mounted,
            Phase::Pending(_) => Outcome::Unattached,
            Phase::Claimed | Phase::Mounted => Outcome::Failed,
        };
        entries.remove(offer_id).unwrap().finish(outcome);
        MountStatus::Unavailable
    }

    pub(super) fn is_mounted(
        &self, label: &str, document: u64, parent: &str, offer_id: &str, handle: &str,
    ) -> bool {
        label == MAIN && self.0.lock().unwrap().get(offer_id).is_some_and(|entry| {
            entry.accepts(document) && entry.session_id == parent && entry.handle == handle
                && matches!(entry.phase, Phase::Mounted)
        })
    }

    pub(super) fn offer(
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

    pub(super) fn file_closed(
        &self, label: &str, document: u64, parent: &str, offer_id: &str, handle: &str,
    ) {
        if label != MAIN {
            return;
        }
        let mut entries = self.0.lock().unwrap();
        let Some(entry) = entries.get(offer_id) else { return; };
        if entry.document != document || entry.session_id != parent || entry.handle != handle {
            return;
        }
        let outcome = match entry.phase {
            Phase::Pending(_) => Outcome::Unattached,
            Phase::Mounted if entry.accepts(document) => Outcome::Closed,
            Phase::Claimed | Phase::Mounted => Outcome::Failed,
        };
        entries.remove(offer_id).unwrap().finish(outcome);
    }
}

async fn open(
    app: &AppHandle, slot: &BridgeSlot, document: u64, parent: &str, active: ActiveOpen,
    request: &CancellationToken, work: mpsc::Sender<PublicationWork>,
) -> Outcome {
    let Ok(path) = std::str::from_utf8(&active.path) else { return Outcome::Unattached; };
    let sftp = app.state::<SftpIpc>();
    let sessions = app.state::<Sessions>();
    let slot_cancel = slot.cancellation();
    if request.is_cancelled() || slot_cancel.is_cancelled() || sessions.document(MAIN) != document {
        return Outcome::Unattached;
    }
    let offer_id = uuid::Uuid::new_v4().simple().to_string();
    let publisher = Publisher::new(active.id, request.clone(), work);
    let Ok(opened) = sftp.open_verified_bridge(slot, &sessions, path, &offer_id, active.snapshot, publisher).await else {
        return Outcome::Unattached;
    };
    let handle = opened.handle.clone();
    let state = app.state::<NanoBridge>();
    if request.is_cancelled() || slot_cancel.is_cancelled() {
        sftp.discard_bridge_file(slot, &offer_id, &handle).await;
        return Outcome::Unattached;
    }
    let (offer, mut done) = state.offer(offer_id.clone(), document, parent, opened, request.clone(), slot_cancel.clone());
    if app.emit_to(EventTarget::webview(MAIN), "editor-remote-nano-open", &offer).is_err() {
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
    if outcome == Outcome::Failed && sessions.document(MAIN) == document {
        let _ = app.emit_to(EventTarget::webview(MAIN), "editor-remote-nano-disconnected", &offer);
    }
    if outcome != Outcome::Closed {
        sftp.discard_bridge_file(slot, &offer_id, &handle).await;
    }
    outcome
}

async fn drive_open(
    id: u64, opened: impl Future<Output = Outcome>, request: &CancellationToken,
    cancel: &CancellationToken, slot_cancel: &CancellationToken, requests: &mut mpsc::Receiver<BridgeRequest>,
) -> Option<Outcome> {
    tokio::pin!(opened);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            outcome = &mut opened => return Some(outcome),
            message = requests.recv() => match message {
                Some(BridgeRequest::Cancel(cancelled)) if cancelled == id => request.cancel(),
                Some(BridgeRequest::Cancel(_)) => {},
                None | Some(BridgeRequest::Open(_)) => break,
            },
        }
    }
    request.cancel();
    slot_cancel.cancel();
    opened.await;
    None
}

pub(crate) enum BridgeRequest {
    Open(ActiveOpen),
    Cancel(u64),
}

// 배치 공개키 인증과 관리자가 확인한 셸·SFTP 공통 경로를 선택한 탭에서만 호출한다.
pub(crate) async fn run(
    app: AppHandle, owner: String, document: u64, parent: String, cancel: CancellationToken,
    mut requests: mpsc::Receiver<BridgeRequest>, responses: mpsc::Sender<Response>, work: mpsc::Sender<PublicationWork>,
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
        let active = match message {
            Some(BridgeRequest::Open(active)) => active,
            Some(BridgeRequest::Cancel(_)) => continue,
            None => return,
        };
        let id = active.id;
        let outcome = if let Some(slot) = &slot {
            let request = cancel.child_token();
            let slot_cancel = slot.cancellation();
            let opened = open(&app, slot, document, &parent, active, &request, work.clone());
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
