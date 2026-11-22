//! 사용자가 조작하는 탭 하나의 RFB 세션. 연결 경계와 이벤트 계약은 `README.md` 참고.

mod approved;
pub(crate) mod ipc;
mod lifetime;
pub(crate) mod output;
#[cfg(windows)]
mod password_dialog;
#[cfg(target_os = "linux")]
mod vault;
mod presentation;

pub(crate) mod transport;
mod wire;

use transport::{Auth, Route, Secrets, Target};
#[cfg(test)]
use transport::{TunnelRegistry, VerifiedTunnel};

use std::{
    io,
    num::NonZeroU64,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, sleep_until, timeout},
};
use transport::Stream;
use wire::{Message, Screen, Update};

const IO_TIMEOUT: Duration = Duration::from_secs(10);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionId(NonZeroU64);

impl SessionId {
    pub(crate) fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Size {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Rect {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) size: Size,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Error {
    UnverifiedRoute,
    TlsIdentity,
    UnsupportedVersion,
    SecurityType,
    MissingSecret,
    InvalidSecret,
    Authentication,
    InvalidFrame,
    LengthLimit,
    UnsupportedEncoding,
    UnsupportedMessage,
    InvalidPointer,
    InvalidCutText,
    ScreenNotReady,
    Busy,
    Disconnected,
    Timeout,
    ResizeTimeout,
    IdExhausted,
    Io(io::ErrorKind),
}

pub(crate) enum Input {
    Key { keysym: u32, down: bool },
    Pointer { x: u16, y: u16, buttons: u8 },
    CutText(String),
    Resize(Size),
    Release,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Change {
    Size(Size),
    Pixels { rect: Rect, rgba: Vec<u8> },
    Copy { source: Rect, destination: Rect },
    Ready,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ResizeResult {
    Applied(Size),
    Rejected(u16),
    NotNegotiated,
    Busy,
    Unchanged,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Event {
    Frame(Vec<Change>),
    CutText(String),
    Resize(ResizeResult),
    InputRejected(Error),
    #[cfg(test)]
    Closed(Error),
}

#[derive(Default)]
pub(crate) struct Tab {
    current: Option<Session>,
}

struct Session {
    id: SessionId,
    input: mpsc::Sender<(u64, Input)>,
    screen_epoch: Arc<AtomicU64>,
    events: mpsc::Receiver<Event>,
    controls: mpsc::Receiver<Event>,
    closed: oneshot::Receiver<Error>,
    task: JoinHandle<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Tab {
    pub(crate) async fn open(
        &mut self,
        target: &Target,
        route: Route<'_>,
        auth: Auth,
        secrets: &impl Secrets,
    ) -> Result<SessionId, Error> {
        self.stop().await;
        let mut stream = timeout(IO_TIMEOUT, transport::connect(target, route)).await.map_err(|_| Error::Timeout)??;
        let size = wire::handshake(&mut stream, target, auth, secrets).await?;
        self.start(stream, size)
    }

    fn start(&mut self, stream: Box<dyn Stream>, size: Size) -> Result<SessionId, Error> {
        // fetch_update는 프로젝트의 Rust 2024 최소 버전에서도 제공된다.
        #[allow(deprecated)]
        let serial = NEXT_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| Error::IdExhausted)?;
        let id = SessionId(NonZeroU64::new(serial).ok_or(Error::IdExhausted)?);
        let (input, inputs) = mpsc::channel(32);
        let (output, events) = mpsc::channel(1);
        let (control, controls) = mpsc::channel(4);
        let (close, closed) = oneshot::channel();
        // 짝수는 화면 대기, 홀수는 입력 가능한 화면 세대다.
        let screen_epoch = Arc::new(AtomicU64::new(0));
        let worker_epoch = Arc::clone(&screen_epoch);
        let task = tokio::spawn(async move {
            let error = run(stream, size, inputs, &output, &control, &worker_epoch)
                .await.err().unwrap_or(Error::Disconnected);
            let _ = close.send(error);
        });
        self.current = Some(Session {
            id,
            input,
            screen_epoch,
            events,
            controls,
            closed,
            task,
        });
        Ok(id)
    }

    #[cfg(test)]
    pub(crate) fn input(&self, id: SessionId, input: Input) -> Result<(), Error> {
        let current = self.current.as_ref().filter(|session| session.id == id)
            .ok_or(Error::Disconnected)?;
        enqueue(&current.input, &current.screen_epoch, current.screen_epoch.load(Ordering::Acquire), input)
    }

    #[cfg(test)]
    pub(crate) async fn next(&mut self, id: SessionId) -> Result<Event, Error> {
        let current = self
            .current
            .as_mut()
            .filter(|session| session.id == id)
            .ok_or(Error::Disconnected)?;
        let event = tokio::select! {
            biased;
            Some(event) = current.events.recv() => event,
            Some(event) = current.controls.recv() => event,
            error = &mut current.closed => Event::Closed(error.unwrap_or(Error::Disconnected)),
        };
        if matches!(event, Event::Closed(_)) {
            self.current = None;
        }
        Ok(event)
    }

    async fn stop(&mut self) {
        if let Some(mut previous) = self.current.take() {
            previous.task.abort();
            let _ = (&mut previous.task).await;
        }
    }
}

fn enqueue(inputs: &mpsc::Sender<(u64, Input)>, screen_epoch: &AtomicU64, epoch: u64, input: Input) -> Result<(), Error> {
    if inputs.is_closed() {
        return Err(Error::Disconnected);
    }
    if epoch % 2 == 0 || screen_epoch.load(Ordering::Acquire) != epoch {
        return Err(Error::ScreenNotReady);
    }
    inputs.try_send((epoch, input)).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => Error::Busy,
        mpsc::error::TrySendError::Closed(_) => Error::Disconnected,
    })
}

async fn run(
    stream: Box<dyn Stream>,
    size: Size,
    mut inputs: mpsc::Receiver<(u64, Input)>,
    output: &mpsc::Sender<Event>,
    control: &mpsc::Sender<Event>,
    screen_epoch: &AtomicU64,
) -> Result<(), Error> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut frame = Frame::new(size)?;
    let mut screen: Option<Screen> = None;
    let mut pending: Option<(Screen, Instant)> = None;
    let mut held = std::collections::BTreeSet::new();
    let mut pointer = (0, 0, 0);
    let mut frame_event = Some(Event::Frame(vec![Change::Size(size)]));
    loop {
        let message = {
            // 입력을 보내는 동안 부분 수신 중인 메시지의 진행 상태를 유지한다.
            let reading = wire::read_message(&mut reader, frame.size);
            tokio::pin!(reading);
            loop {
                let deadline = async {
                    match pending {
                        Some((_, at)) => sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    biased;
                    _ = deadline => return Err(Error::ResizeTimeout),
                    permit = output.reserve(), if frame_event.is_some() => {
                        permit.map_err(|_| Error::Disconnected)?.send(frame_event.take().unwrap());
                    }
                    input = inputs.recv() => {
                        let (epoch, input) = input.ok_or(Error::Disconnected)?;
                        if !frame.ready || epoch != screen_epoch.load(Ordering::Relaxed) {
                            continue;
                        }
                        timeout(IO_TIMEOUT, async {
                            match input {
                                Input::Resize(size) => {
                                    let result = if pending.is_some() {
                                        ResizeResult::Busy
                                    } else if let Some(mut current) = screen {
                                        if let Err(error) = size.pixels() {
                                            return send_control(control, Event::InputRejected(error));
                                        }
                                        if size == frame.size {
                                            ResizeResult::Unchanged
                                        } else {
                                            current.bounds.size = size;
                                            wire::request_resize(&mut writer, current).await?;
                                            pending = Some((current, Instant::now() + IO_TIMEOUT));
                                            return Ok(());
                                        }
                                    } else {
                                        ResizeResult::NotNegotiated
                                    };
                                    send_control(control, Event::Resize(result))?;
                                }
                                Input::Pointer { x, y, buttons } => {
                                    if x >= frame.size.width || y >= frame.size.height {
                                        return send_control(control, Event::InputRejected(Error::InvalidPointer));
                                    }
                                    let [x0, x1] = x.to_be_bytes();
                                    let [y0, y1] = y.to_be_bytes();
                                    pointer = (x, y, buttons);
                                    writer.write_all(&[5, buttons, x0, x1, y0, y1]).await?;
                                }
                                Input::Key { keysym, down } => {
                                    let [k0, k1, k2, k3] = keysym.to_be_bytes();
                                    if down && !held.contains(&keysym) && held.len() >= 256 {
                                        return Err(Error::LengthLimit);
                                    }
                                    if down { held.insert(keysym); } else { held.remove(&keysym); }
                                    writer.write_all(&[4, u8::from(down), 0, 0, k0, k1, k2, k3]).await?;
                                }
                                Input::CutText(text) => {
                                    if text.len() > wire::MAX_TEXT * 2 {
                                        return send_control(control, Event::InputRejected(Error::InvalidCutText));
                                    }
                                    let bytes = text.chars().map(|ch| u8::try_from(u32::from(ch))).collect::<Result<Vec<_>, _>>();
                                    let Ok(bytes) = bytes else { return send_control(control, Event::InputRejected(Error::InvalidCutText)); };
                                    if bytes.len() > wire::MAX_TEXT {
                                        return send_control(control, Event::InputRejected(Error::InvalidCutText));
                                    }
                                    writer.write_all(&[6, 0, 0, 0]).await?;
                                    writer.write_u32(bytes.len() as u32).await?;
                                    writer.write_all(&bytes).await?;
                                }
                                Input::Release => release(&mut writer, &mut held, &mut pointer).await?,
                            }
                            Ok(())
                        }).await.map_err(|_| Error::Timeout)??;
                    }
                    message = &mut reading, if frame_event.is_none() => break message?,
                }
            }
        };
        match message {
            Message::Bell => {}
            Message::CutText(text) => send_control(control, Event::CutText(text))?,
            Message::Frame(updates) => {
                let mut changes = Vec::new();
                let mut resize = None;
                let mut layout = None;
                let old_epoch = screen_epoch.load(Ordering::Relaxed);
                let mut size_changed = false;
                for update in updates {
                    match update {
                        Update::Pixels(rect, rgba) => {
                            let ready = frame.cover(rect);
                            changes.push(Change::Pixels { rect, rgba });
                            if ready {
                                changes.push(Change::Ready);
                            }
                        }
                        Update::Copy {
                            source,
                            destination,
                        } => {
                            if !frame.ready {
                                return Err(Error::InvalidFrame);
                            }
                            changes.push(Change::Copy {
                                source,
                                destination,
                            });
                        }
                        Update::LegacySize(size) => {
                            size_changed |= frame.resize(size, &mut changes)?;
                            screen = None;
                        }
                        Update::Layout {
                            reply,
                            applied,
                            single_screen,
                        } => {
                            if let Some(code) = reply {
                                let (requested, _) = pending.take().ok_or(Error::InvalidFrame)?;
                                resize = Some(if code == 0 {
                                    if single_screen != Some(requested)
                                        || applied != Some(requested.bounds.size)
                                    {
                                        return Err(Error::InvalidFrame);
                                    }
                                    ResizeResult::Applied(requested.bounds.size)
                                } else {
                                    ResizeResult::Rejected(code)
                                });
                            }
                            if let Some(size) = applied {
                                layout = Some((size, single_screen));
                            }
                        }
                    }
                }
                if let Some((size, single_screen)) = layout {
                    size_changed |= frame.resize(size, &mut changes)?;
                    screen = single_screen.filter(|s| s.bounds == (Rect { x: 0, y: 0, size }));
                }
                let epoch =
                    ((old_epoch & !1) + 2 * u64::from(size_changed)) | u64::from(frame.ready);
                if epoch != old_epoch {
                    screen_epoch.store(epoch, Ordering::Release);
                    if size_changed {
                        let (x, y, _) = &mut pointer;
                        *x = (*x).min(frame.size.width - 1);
                        *y = (*y).min(frame.size.height - 1);
                        timeout(IO_TIMEOUT, release(&mut writer, &mut held, &mut pointer))
                            .await.map_err(|_| Error::Timeout)??;
                    }
                }
                if !changes.is_empty() {
                    let event = Event::Frame(changes);
                    match output.try_send(event) {
                        Ok(()) => {},
                        Err(mpsc::error::TrySendError::Full(event)) => frame_event = Some(event),
                        Err(mpsc::error::TrySendError::Closed(_)) => return Err(Error::Disconnected),
                    }
                }
                if let Some(result) = resize {
                    send_control(control, Event::Resize(result))?;
                }
                timeout(
                    IO_TIMEOUT,
                    wire::request_frame(&mut writer, frame.size, !size_changed),
                )
                .await
                .map_err(|_| Error::Timeout)??;
            }
        }
    }
}

async fn release(
    writer: &mut (impl AsyncWriteExt + Unpin),
    held: &mut std::collections::BTreeSet<u32>,
    (x, y, buttons): &mut (u16, u16, u8),
) -> Result<(), Error> {
    for key in std::mem::take(held) {
        let [a, b, c, d] = key.to_be_bytes();
        writer.write_all(&[4, 0, 0, 0, a, b, c, d]).await?;
    }
    if std::mem::take(buttons) != 0 {
        let [a, b] = x.to_be_bytes();
        let [c, d] = y.to_be_bytes();
        writer.write_all(&[5, 0, a, b, c, d]).await?;
    }
    Ok(())
}

fn send_control(output: &mpsc::Sender<Event>, event: Event) -> Result<(), Error> {
    output.try_send(event).map_err(|_| Error::Disconnected)
}

struct Frame {
    size: Size,
    ready: bool,
    uncovered: usize,
    covered: Vec<u64>,
}

impl Frame {
    fn new(size: Size) -> Result<Self, Error> {
        Ok(Self {
            size,
            ready: false,
            uncovered: size.pixels()?,
            covered: Vec::new(),
        })
    }

    fn resize(&mut self, size: Size, changes: &mut Vec<Change>) -> Result<bool, Error> {
        if self.size == size {
            return Ok(false);
        }
        *self = Self::new(size)?;
        changes.push(Change::Size(size));
        Ok(true)
    }

    fn cover(&mut self, rect: Rect) -> bool {
        if self.ready {
            return false;
        }
        if rect.size != self.size {
            let width = usize::from(self.size.width);
            if self.covered.is_empty() {
                self.covered.resize(self.uncovered.div_ceil(64), 0);
            }
            for y in rect.y..rect.y + rect.size.height {
                let start = usize::from(y) * width + usize::from(rect.x);
                let end = start + usize::from(rect.size.width);
                for index in start / 64..=(end - 1) / 64 {
                    let base = index * 64;
                    let low = start.saturating_sub(base);
                    let high = (end - base).min(64);
                    let mask = (u64::MAX << low) & (u64::MAX >> (64 - high));
                    let word = &mut self.covered[index];
                    self.uncovered -= (mask & !*word).count_ones() as usize;
                    *word |= mask;
                }
            }
            if self.uncovered != 0 {
                return false;
            }
        }
        self.covered = Vec::new();
        self.ready = true;
        true
    }
}

#[cfg(test)]
mod tests;
