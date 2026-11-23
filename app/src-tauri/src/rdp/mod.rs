mod clipboard;
mod connect;
mod frame;
pub(crate) mod ipc;
mod native;
pub(crate) mod route;
mod trust;

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use ironrdp::{
    cliprdr::CliprdrClient,
    connector::{
        DesktopSize,
        connection_activation::{ConnectionActivationFactory, ConnectionActivationState},
    },
    displaycontrol::client::DisplayControlClient,
    dvc::DrdynvcClient,
    graphics::image_processing::PixelFormat,
    pdu::input::{
        fast_path::{FastPathInputEvent, KeyboardFlags},
        mouse::{MousePdu, PointerFlags},
    },
    session::{
        ActiveStage, ActiveStageBuilder, ActiveStageOutput, fast_path::ProcessorBuilder,
        image::DecodedImage,
    },
};
use ironrdp_tokio::FramedWrite as _;
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::{sleep_until, timeout},
};
use tokio_rustls::rustls::{self, pki_types::ServerName};

pub use frame::{Frame, Rect};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_LIFETIME: Duration = Duration::from_secs(8 * 60 * 60);
static SLOTS: Semaphore = Semaphore::const_new(4);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionId(u64);

/// The remote DNS identity does not change when the TCP stream comes from a tunnel.
struct Target {
    endpoint: route::Endpoint,
    server_name: String,
    size: DesktopSize,
    /// 사용자가 승인한 경우에만 NLA 없는 서버에 TLS 전용으로 한 번 재연결한다.
    tls_only: bool,
}

/// Local authentication data; never serialized or sent to the renderer.
struct Account {
    username: String,
    domain: Option<String>,
    password: zeroize::Zeroizing<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Bounds,
    Busy,
    ChannelUnavailable,
    Clipboard,
    Closed,
    Credentials,
    InvalidServerName,
    InvalidForwardDestination,
    Tcp,
    Negotiation,
    TlsOnlyRequired,
    TrustStore,
    TlsIdentity,
    Authentication,
    Activation,
    Protocol,
    StaleDisplay,
    Transport,
    Timeout,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Channels {
    /// MCS assignment, then the server's CLIPRDR initialization and current text advertisement.
    pub cliprdr_mcs_id: Option<u16>,
    pub cliprdr_ready: bool,
    pub remote_clipboard_text: bool,
    /// Server-opened DVC and the area announced in Display Control capabilities.
    pub display_dvc_id: Option<u32>,
    pub display_max_area: Option<u64>,
}

pub enum Event {
    Frame(Frame),
    Channels(Channels),
    ClipboardText(Result<String, Error>),
}

pub enum Input {
    ScanCode {
        code: u8,
        extended: bool,
        released: bool,
    },
    Pointer {
        x: u16,
        y: u16,
        action: Pointer,
    },
    OfferClipboardText(String),
    ReadClipboardText,
    Resize {
        width: u16,
        height: u16,
    },
}

#[derive(Clone, Copy)]
pub enum Pointer {
    Move,
    Button { button: MouseButton, pressed: bool },
    Wheel { units: i16, horizontal: bool },
}

#[derive(Clone, Copy)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

type Command = (Option<u64>, Input, oneshot::Sender<Result<(), Error>>);

/// The owning tab must close this session before dropping its tunnel or reconnecting.
/// Send input only from the focused tab. A fresh session owns a fresh command queue.
pub struct Session {
    id: SessionId,
    input: mpsc::Sender<Command>,
    events: mpsc::Receiver<(u64, Event)>,
    task: JoinHandle<Result<(), Error>>,
    display_generation: u64,
}

/// Clone for focused input while the session owner awaits events.
#[derive(Clone)]
pub struct InputHandle {
    id: SessionId,
    input: mpsc::Sender<Command>,
}

impl Session {
    /// Resolve the NLA account only after TLS verifies the remote DNS identity.
    /// A successful return starts event delivery; the first Frame is the display-ready signal.
    /// `approved_roots` adds Rust-approved DER CAs to the platform roots, never WebView input.
    async fn open<F, Fut, A, AFut>(
        target: Target,
        approved_roots: rustls::RootCertStore,
        trust: trust::Trust<A>,
        resolve: F,
        auth: ipc::AuthIo<'_>,
    ) -> Result<Self, Error>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Account, Error>>,
        A: FnOnce(trust::Request) -> AFut,
        AFut: Future<Output = Result<bool, Error>>,
    {
        frame::frame_len(target.size.width, target.size.height)?;
        if !route::dns_name(&target.server_name) {
            return Err(Error::InvalidServerName);
        }
        let name = ServerName::try_from(target.server_name.clone())
            .map_err(|_| Error::InvalidServerName)?;
        let permit = SLOTS.try_acquire().map_err(|_| Error::Busy)?;
        let roots = connect::trusted_roots(approved_roots)?;
        Self::open_with_tls(target, name, connect::tls_config(roots), trust, resolve, permit, auth).await
    }

    async fn open_with_tls<F, Fut, A, AFut>(
        target: Target,
        name: ServerName<'static>,
        tls: connect::Tls,
        trust: trust::Trust<A>,
        resolve: F,
        permit: tokio::sync::SemaphorePermit<'static>,
        auth: ipc::AuthIo<'_>,
    ) -> Result<Self, Error>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Account, Error>>,
        A: FnOnce(trust::Request) -> AFut,
        AFut: Future<Output = Result<bool, Error>>,
    {
        let area = Arc::new(AtomicU64::new(0));
        let display = DisplayControlClient::new({
            let area = Arc::clone(&area);
            move |caps| {
                area.store(caps.max_monitor_area(), Ordering::Relaxed);
                Ok(Vec::new())
            }
        });
        let (result, mut transport) = connect::connect(target, name, tls, trust, resolve, display, auth).await?;
        let id = SessionId(
            NEXT_ID
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .map_err(|_| Error::Busy)?,
        );
        let mut inbound = frame::Inbound::default();
        inbound.io_id = result.io_channel_id;
        inbound.clipboard_id = result
            .static_channels
            .get_channel_id_by_type::<CliprdrClient>();
        inbound.display_carrier_id = result
            .static_channels
            .get_channel_id_by_type::<DrdynvcClient>();
        let mut graphics = Graphics::new(result.desktop_size)?;
        let mut active = ActiveStageBuilder {
            static_channels: result.static_channels,
            user_channel_id: result.user_channel_id,
            io_channel_id: result.io_channel_id,
            message_channel_id: result.message_channel_id,
            share_id: result.share_id,
            compression_type: result.compression_type,
            enable_server_pointer: result.enable_server_pointer,
            pointer_software_rendering: result.pointer_software_rendering,
        }
        .build();
        let (tx, mut rx) = mpsc::channel::<Command>(1);
        let (event_tx, events) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            let _permit = permit;
            timeout(MAX_LIFETIME, async {
                let mut channels = Channels::default();
                let mut pending = VecDeque::new();
                let mut display_generation = 1_u64;
                let mut normalizer = frame::Normalizer::default();
                loop {
                    let clipboard_deadline = clipboard::deadline(&mut active);
                    tokio::select! {
                        permit = event_tx.reserve(), if !pending.is_empty() => {
                            let event = pending.pop_front().expect("pending event");
                            if matches!(event, Event::Frame(_)) { graphics.ready = true; }
                            permit.map_err(|_| Error::Closed)?.send((display_generation, event));
                        }
                        () = wait_deadline(clipboard_deadline) => {
                            if let Some(event) = clipboard::expire(&mut active) { pending.push_back(event); }
                        }
                        next = connect::read_bounded(&mut transport, &ironrdp::pdu::RDP_HINT, Error::Transport, None), if pending.is_empty() => {
                            let pdu = next?;
                            let action = ironrdp::pdu::find_size(&pdu).map_err(|_| Error::Protocol)?.ok_or(Error::Protocol)?.action;
                            let pdus = match normalizer.normalize(action, &pdu, &graphics.image)? {
                                frame::Normalized::Pass => vec![(action, pdu)],
                                frame::Normalized::Drop => Vec::new(),
                                frame::Normalized::Replace(pdus) => pdus,
                            };
                            for (action, pdu) in pdus {
                                let pixels = inbound.check(action, &pdu, &graphics.image)?;
                                let outputs = active.process(&mut graphics.image, action, &pdu).map_err(|_| Error::Protocol)?;
                                let mut dirty = None;
                                for output in outputs {
                                    match output {
                                        ActiveStageOutput::ResponseFrame(data) => write(&mut transport, &data).await?,
                                        ActiveStageOutput::GraphicsUpdate(rect) if pixels => {
                                            use ironrdp::pdu::geometry::{InclusiveRectangle, Rectangle as _};
                                            dirty = Some(dirty.map_or_else(|| rect.clone(), |previous: InclusiveRectangle| previous.union(&rect)));
                                        }
                                        ActiveStageOutput::DeactivateAll => {
                                            graphics = timeout(IO_TIMEOUT, reactivate(&mut transport, &mut active, &result.activation_factory))
                                                .await.map_err(|_| Error::Timeout)??;
                                            display_generation += 1;
                                            inbound.reset_graphics();
                                            dirty = None;
                                        }
                                        ActiveStageOutput::Terminate(_) => return Ok(()),
                                        _ => {}
                                    }
                                }
                                if let Some(dirty) = dirty {
                                    if let Some(frame) = graphics.update(id, &dirty)? { pending.push_back(Event::Frame(frame)); }
                                }
                            }
                            let next_channels = channel_state(&mut active, inbound.clipboard_id, &area);
                            if next_channels != channels {
                                channels = next_channels;
                                pending.push_back(Event::Channels(channels));
                            }
                            let (reply, event) = clipboard::received(&mut active)?;
                            write(&mut transport, &reply).await?;
                            if let Some(event) = event { pending.push_back(event); }
                        }
                        command = rx.recv() => {
                            let Some((generation, command, ack)) = command else { return Ok(()) };
                            if ack.is_closed() { continue; }
                            if generation.is_some_and(|generation| generation != display_generation) {
                                let _ = ack.send(Err(Error::StaleDisplay));
                                continue;
                            }
                            if matches!(command, Input::ReadClipboardText) && !pending.is_empty() {
                                let _ = ack.send(Err(Error::Busy));
                                continue;
                            }
                            let data = encode(&mut active, &graphics, channels, command);
                            let outcome = match data {
                                Ok(data) => write(&mut transport, &data).await,
                                Err(error) => Err(error),
                            };
                            let _ = ack.send(outcome);
                            if matches!(outcome, Err(Error::Transport | Error::Timeout)) { outcome?; }
                        }
                    }
                }
            }).await.map_err(|_| Error::Timeout)?
        });
        Ok(Self {
            id,
            input: tx,
            events,
            task,
            display_generation: 0,
        })
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn input_handle(&self) -> InputHandle {
        InputHandle {
            id: self.id,
            input: self.input.clone(),
        }
    }

    pub async fn input(&self, id: SessionId, input: Input) -> Result<(), Error> {
        self.input_handle().input(id, input).await
    }

    /// Drain events to resume server reads; a full queue never blocks local input or timers.
    pub async fn next(&mut self) -> Option<Event> {
        let (generation, event) = self.events.recv().await?;
        self.display_generation = generation;
        Some(event)
    }

    /// Cancels the task and waits for the socket and queued input to be dropped.
    pub async fn close(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }

    /// Call once after `next` returns `None`. Reports the terminal stage without secret data.
    pub async fn finish(mut self) -> Result<(), Error> {
        self.events.close();
        (&mut self.task).await.map_err(|_| Error::Closed)?
    }
}

impl InputHandle {
    /// Takes one bounded command; rejects mismatched IDs and full input queues.
    pub async fn input(&self, id: SessionId, input: Input) -> Result<(), Error> {
        self.send(id, None, input).await
    }

    async fn send(&self, id: SessionId, generation: Option<u64>, input: Input) -> Result<(), Error> {
        if id != self.id {
            return Err(Error::Closed);
        }
        if let Input::OfferClipboardText(text) = &input {
            if text.len() > clipboard::MAX_TEXT || text.contains('\0') {
                return Err(Error::Bounds);
            }
        }
        let (tx, rx) = oneshot::channel();
        self.input
            .try_send((generation, input, tx))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::Busy,
                mpsc::error::TrySendError::Closed(_) => Error::Closed,
            })?;
        rx.await.map_err(|_| Error::Closed)?
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Graphics {
    image: DecodedImage,
    ready: bool,
}

impl Graphics {
    fn new(size: DesktopSize) -> Result<Self, Error> {
        frame::frame_len(size.width, size.height)?;
        Ok(Self {
            image: DecodedImage::new(PixelFormat::RgbA32, size.width, size.height),
            ready: false,
        })
    }

    fn update(
        &self,
        id: SessionId,
        dirty: &ironrdp::pdu::geometry::InclusiveRectangle,
    ) -> Result<Option<Frame>, Error> {
        // IronRDP can report a rectangle after ignoring a compressed bitmap it could not decode.
        // RGBA starts transparent and every supported successful bitmap write sets alpha opaque.
        if !self.ready && !frame::has_pixels(&self.image, dirty)? {
            return Ok(None);
        }
        let event = frame::patch(id, &self.image, dirty, !self.ready)?;
        Ok(Some(event))
    }
}

async fn write(transport: &mut connect::Transport, data: &[u8]) -> Result<(), Error> {
    if !data.is_empty() {
        timeout(IO_TIMEOUT, transport.write_all(data))
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|_| Error::Transport)?;
    }
    Ok(())
}

async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn reactivate(
    transport: &mut connect::Transport,
    stage: &mut ActiveStage,
    factory: &ConnectionActivationFactory,
) -> Result<Graphics, Error> {
    let mut sequence = factory.create();
    loop {
        connect::step(transport, &mut sequence, Error::Activation).await?;
        if let ConnectionActivationState::Finalized {
            desktop_size,
            share_id,
            enable_server_pointer,
            pointer_software_rendering,
        } = sequence.connection_activation_state()
        {
            let graphics = Graphics::new(desktop_size)?;
            stage.set_fastpath_processor(
                ProcessorBuilder {
                    io_channel_id: sequence.io_channel_id(),
                    user_channel_id: sequence.user_channel_id(),
                    share_id,
                    enable_server_pointer,
                    pointer_software_rendering,
                    bulk_decompressor: None,
                }
                .build(),
            );
            stage.set_share_id(share_id);
            stage.set_enable_server_pointer(enable_server_pointer);
            return Ok(graphics);
        }
    }
}

fn channel_state(
    stage: &mut ActiveStage,
    cliprdr_mcs_id: Option<u16>,
    area: &AtomicU64,
) -> Channels {
    let mut state = Channels {
        cliprdr_mcs_id,
        ..Default::default()
    };
    if let Some(channel) = stage.get_svc_processor_mut::<CliprdrClient>() {
        let clip = clipboard::backend(channel);
        state.cliprdr_ready = clip.ready;
        state.remote_clipboard_text = clip.remote_text;
    }
    if let Some(channel) = stage.get_dvc::<DisplayControlClient>() {
        state.display_dvc_id = channel.channel_id();
        if state.display_dvc_id.is_some()
            && channel
                .channel_processor_downcast_ref::<DisplayControlClient>()
                .is_some_and(DisplayControlClient::ready)
        {
            state.display_max_area = Some(area.load(Ordering::Relaxed));
        }
    }
    state
}

fn encode(
    active: &mut ActiveStage,
    graphics: &Graphics,
    channels: Channels,
    command: Input,
) -> Result<Vec<u8>, Error> {
    use ironrdp::core::encode_vec;
    use ironrdp::pdu::input::fast_path::FastPathInput;

    if !graphics.ready {
        return Err(Error::Busy);
    }
    let input = match command {
        Input::ScanCode {
            code,
            extended,
            released,
        } => {
            if code == 0 || code > 0x7f {
                return Err(Error::Bounds);
            }
            let mut flags = KeyboardFlags::empty();
            flags.set(KeyboardFlags::EXTENDED, extended);
            flags.set(KeyboardFlags::RELEASE, released);
            FastPathInputEvent::KeyboardEvent(flags, code)
        }
        Input::Pointer { x, y, action } => {
            if x >= graphics.image.width() || y >= graphics.image.height() {
                return Err(Error::Bounds);
            }
            let (flags, units) = match action {
                Pointer::Move => (PointerFlags::MOVE, 0),
                Pointer::Button { button, pressed } => {
                    let mut flags = match button {
                        MouseButton::Left => PointerFlags::LEFT_BUTTON,
                        MouseButton::Middle => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
                        MouseButton::Right => PointerFlags::RIGHT_BUTTON,
                    };
                    flags.set(PointerFlags::DOWN, pressed);
                    (flags, 0)
                }
                Pointer::Wheel { units, horizontal } => {
                    if units == 0 || !(-256..=255).contains(&units) {
                        return Err(Error::Bounds);
                    }
                    let flag = if horizontal {
                        PointerFlags::HORIZONTAL_WHEEL
                    } else {
                        PointerFlags::VERTICAL_WHEEL
                    };
                    (flag, units)
                }
            };
            FastPathInputEvent::MouseEvent(MousePdu {
                flags,
                number_of_wheel_rotation_units: units,
                x_position: x,
                y_position: y,
            })
        }
        Input::Resize { width, height } => {
            frame::frame_len(width, height)?;
            let area = channels.display_max_area.ok_or(Error::ChannelUnavailable)?;
            if width < 200
                || height < 200
                || width % 2 != 0
                || u64::from(width) * u64::from(height) > area
            {
                return Err(Error::Bounds);
            }
            return active
                .encode_resize(width.into(), height.into(), None, None)
                .ok_or(Error::ChannelUnavailable)?
                .map_err(|_| Error::Protocol);
        }
        command @ (Input::OfferClipboardText(_) | Input::ReadClipboardText) => {
            return clipboard::command(active, command);
        }
    };
    encode_vec(&FastPathInput::single(input)).map_err(|_| Error::Protocol)
}
