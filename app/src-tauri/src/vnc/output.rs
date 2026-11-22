use std::{sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}}, time::Duration};

use tauri::{Manager, UriSchemeContext, UriSchemeResponder, http::{Request, Response}};
use tokio::sync::oneshot;

use super::{Error, ipc::{Sessions, opaque_id}, lifetime::Lifetime, presentation::CHUNK};

const MAX_BODY: usize = 2 * 1024 * 1024 + 4096;

#[derive(Default)]
pub(super) struct Mailbox {
    lanes: Mutex<[Option<Packet>; 2]>,
    pub(super) painted: AtomicU64,
}

struct Packet {
    ticket: String,
    sequence: u64,
    offset: u32,
    painted_epoch: Option<u64>,
    body: Option<(bool, Vec<u8>)>,
    confirm: oneshot::Sender<()>,
}

// 화면 준비와 바이너리는 화면 레인, 그 외 통지는 제어 레인을 사용한다.
fn lane(sequence: u64, offset: u32) -> usize {
    usize::from(sequence != 0 || offset % 2 == 1)
}

impl Mailbox {
    pub(super) fn clear(&self) {
        let mut lanes = self.lanes.lock().unwrap();
        *lanes = [None, None];
        self.painted.store(0, Ordering::Relaxed);
    }

    pub(super) fn take(&self, ticket: &str) -> Option<(bool, Vec<u8>)> {
        self.lanes.lock().unwrap().iter_mut().flatten().find(|packet| packet.ticket == ticket)?.body.take()
    }

    pub(super) fn ack(&self, sequence: u64, offset: u32) -> Option<()> {
        let mut lanes = self.lanes.lock().unwrap();
        let pending = &mut lanes[lane(sequence, offset)];
        if pending.as_ref().is_none_or(|packet| (packet.sequence, packet.offset) != (sequence, offset) || packet.body.is_some()) { return None; }
        let packet = pending.take()?;
        // ACK 응답 직후 입력이 와도 화면 세대가 이미 반영되어 있어야 한다.
        if let Some(epoch) = packet.painted_epoch { self.painted.store(epoch, Ordering::Relaxed); }
        let _ = packet.confirm.send(());
        Some(())
    }
}

pub(super) struct Output {
    pub life: Arc<Lifetime>,
    pub mailbox: Arc<Mailbox>,
    pub notify: tauri::ipc::Channel<String>,
}

impl Output {
    pub(super) async fn send(&self, raw: bool, bytes: Vec<u8>, sequence: u64, offset: u32, painted_epoch: Option<u64>) -> Result<(), Error> {
        if bytes.len() > if raw { CHUNK + 33 } else { MAX_BODY } { return Err(Error::LengthLimit); }
        let ticket = opaque_id().map_err(|_| Error::Disconnected)?;
        let (confirm, reply) = oneshot::channel();
        self.life.live(|| {
            let mut lanes = self.mailbox.lanes.lock().unwrap();
            let pending = &mut lanes[lane(sequence, offset)];
            if pending.is_some() { return Err(Error::Busy); }
            *pending = Some(Packet { ticket: ticket.clone(), sequence, offset, painted_epoch, body: Some((raw, bytes)), confirm });
            self.notify.send(ticket).map_err(|_| Error::Disconnected)
        }).ok_or(Error::Disconnected)??;
        tokio::time::timeout(Duration::from_secs(10), reply).await.map_err(|_| Error::Timeout)?.map_err(|_| Error::Disconnected)
    }
}

pub(crate) fn protocol(ctx: UriSchemeContext<'_, tauri::Wry>, request: Request<Vec<u8>>, responder: UriSchemeResponder) {
    let mut responder = Some(responder);
    let origin = request.headers().get("origin");
    let _ = (|| {
        if ctx.webview_label() != "main" || request.method() != "GET" { return None; }
        let path = request.uri().path().strip_prefix('/')?;
        let (document, ticket) = path.split_once('-')?;
        if document.len() != 32 || ticket.len() != 32 { return None; }
        let origin = origin?.to_str().ok()?;
        ctx.app_handle().state::<Sessions>().packet(document, origin, ticket, |raw, bytes| {
            let response = Response::builder().header("Cache-Control", "no-store")
                .header("Access-Control-Allow-Origin", origin)
                .header("Content-Type", if raw { "application/octet-stream" } else { "application/json" })
                .body(bytes).unwrap();
            responder.take().unwrap().respond(response);
        })
    })();
    if let Some(responder) = responder {
        responder.respond(Response::builder().status(404).header("Cache-Control", "no-store").body(Vec::<u8>::new()).unwrap());
    }
}
