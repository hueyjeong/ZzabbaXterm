use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    sync::oneshot,
    time::{timeout, timeout_at, Instant},
};

use crate::local_ipc::{DEADLINE, Endpoint, MAX_FRAME, SERVICE, encode_frame, read_frame, write_frame};

pub(crate) const OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Serialize)]
#[serde(tag = "method", rename_all = "camelCase")]
enum Outbound {
    Edit { path: PathBuf, wait: bool },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    path: PathBuf,
    wait: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Response {
    Accepted,
    Opened,
    Closed,
    Expired,
    OpenFailed,
    AppExited,
    InvalidRequest,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub(crate) edit: Response,
}

impl Request {
    pub(crate) fn is_valid(&self) -> bool {
        valid_path(&self.path)
    }

    pub(crate) fn waits_for_close(&self) -> bool {
        self.wait
    }
}

#[derive(Debug)]
pub enum CliError {
    Usage,
    InvalidPath,
    RequestTooLarge,
    AppNotRunning,
    Disconnected,
    Transport(io::Error),
    InvalidResponse,
    Expired,
    OpenFailed,
    AppExited,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage => f.write_str("사용법: zzabba-edit [--wait] [--] <파일>"),
            Self::InvalidPath => f.write_str("로컬 절대 파일 경로가 필요합니다."),
            Self::RequestTooLarge => f.write_str("파일 열기 요청이 너무 깁니다."),
            Self::AppNotRunning => f.write_str("ZzabbaXterm 앱이 실행 중이 아닙니다."),
            Self::Disconnected => f.write_str("ZzabbaXterm 앱과 연결이 끊겼습니다."),
            Self::Transport(error) => write!(f, "로컬 IPC 오류: {error}"),
            Self::InvalidResponse => f.write_str("ZzabbaXterm 앱의 응답이 올바르지 않습니다."),
            Self::Expired => f.write_str("파일 열기 시간이 초과되었습니다."),
            Self::OpenFailed => f.write_str("앱에서 파일 또는 편집 탭을 열지 못했습니다."),
            Self::AppExited => f.write_str("편집 탭 완료 전에 앱 또는 편집 창이 종료되었습니다."),
        }
    }
}

fn valid_path(path: &Path) -> bool {
    if !path.is_absolute()
        || path.file_name().is_none()
        || !path.to_str().is_some_and(|text| !text.contains('\0'))
    {
        return false;
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        matches!(path.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
            && path
                .file_name()
                .is_some_and(|name| !name.as_encoded_bytes().contains(&b':'))
    }
    #[cfg(not(windows))]
    true
}

fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Outbound, CliError> {
    let mut args = args.into_iter();
    let mut file = args.next().ok_or(CliError::Usage)?;
    let wait = file == "--wait";
    if wait {
        file = args.next().ok_or(CliError::Usage)?;
    }
    if file == "--" {
        file = args.next().ok_or(CliError::Usage)?;
    } else if file.as_encoded_bytes().starts_with(b"-") {
        return Err(CliError::Usage);
    }
    if file.is_empty() || args.next().is_some() {
        return Err(CliError::Usage);
    }
    if file.as_encoded_bytes().len() > MAX_FRAME {
        return Err(CliError::RequestTooLarge);
    }
    let path = std::path::absolute(file).map_err(CliError::Transport)?;
    if !valid_path(&path) {
        return Err(CliError::InvalidPath);
    }
    Ok(Outbound::Edit { path, wait })
}

fn connection_error(error: io::Error) -> CliError {
    match error.kind() {
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted => CliError::Disconnected,
        _ => CliError::Transport(error),
    }
}

fn timed_out(_: tokio::time::error::Elapsed) -> CliError {
    CliError::Transport(io::ErrorKind::TimedOut.into())
}

pub async fn run(args: impl IntoIterator<Item = OsString>) -> Result<(), CliError> {
    let request = parse(args)?;
    let Outbound::Edit { wait, .. } = request;
    let body = encode_frame(&request, MAX_FRAME).map_err(|error| {
        if error.io_error_kind() == Some(io::ErrorKind::FileTooLarge) { CliError::RequestTooLarge }
        else { CliError::InvalidPath }
    })?;
    let endpoint = Endpoint::for_service(SERVICE).map_err(CliError::Transport)?;
    let mut stream = timeout(DEADLINE, endpoint.connect())
        .await
        .map_err(timed_out)?
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => CliError::AppNotRunning,
            _ => connection_error(error),
        })?;
    exchange(&mut stream, &body, wait).await
}

async fn exchange(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    body: &[u8],
    wait: bool,
) -> Result<(), CliError> {
    let deadline = Instant::now() + DEADLINE;
    timeout_at(deadline, write_frame(stream, body, MAX_FRAME))
        .await.map_err(timed_out)?.map_err(connection_error)?;
    let accepted = timeout_at(deadline, read_receipt(stream)).await.map_err(timed_out)??;
    let opened = if accepted == Response::Accepted {
        timeout(OPEN_TIMEOUT + DEADLINE, read_receipt(stream)).await.map_err(timed_out)??
    } else {
        return match accepted {
            Response::OpenFailed => Err(CliError::OpenFailed),
            Response::InvalidRequest => Err(CliError::InvalidPath),
            _ => Err(CliError::InvalidResponse),
        };
    };
    if opened == Response::Opened {
        return if !wait {
            Ok(())
        } else {
            match read_receipt(stream).await? {
                Response::Closed => Ok(()),
                Response::AppExited => Err(CliError::AppExited),
                Response::InvalidRequest => Err(CliError::InvalidPath),
                _ => Err(CliError::InvalidResponse),
            }
        };
    }
    match opened {
        Response::Expired => Err(CliError::Expired),
        Response::OpenFailed => Err(CliError::OpenFailed),
        Response::AppExited => Err(CliError::AppExited),
        Response::InvalidRequest => Err(CliError::InvalidPath),
        _ => Err(CliError::InvalidResponse),
    }
}

async fn read_receipt(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> Result<Response, CliError> {
    let reply = read_frame(stream, MAX_FRAME).await.map_err(|error| match error.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::FileTooLarge => CliError::InvalidResponse,
        _ => connection_error(error),
    })?;
    let Receipt { edit } = serde_json::from_slice(&reply).map_err(|_| CliError::InvalidResponse)?;
    Ok(edit)
}

pub(crate) async fn send_edit(
    stream: &mut (impl AsyncWrite + Unpin),
    edit: Response,
    deadline: Instant,
) -> io::Result<()> {
    let bytes = encode_frame(&Receipt { edit }, MAX_FRAME)?;
    timeout_at(deadline, write_frame(stream, &bytes, MAX_FRAME)).await?
}

// The shared app listener sends Accepted and transfers its edit permit before calling this.
pub(crate) async fn serve_edit<F, Work>(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    request: Request,
    open: F,
) -> io::Result<()>
where
    F: FnOnce(Pending) -> Work,
    Work: Future<Output = ()> + Send + 'static,
{
    let wait = request.waits_for_close();
    let (reply, mut result) = oneshot::channel();
    let (mounted, mut opened) = oneshot::channel();
    let opening = Opening::new(Instant::now() + OPEN_TIMEOUT);
    let mut extra = [0];
    let incoming_error = |incoming: io::Result<usize>| -> io::Result<Response> { match incoming? {
        0 => Err(io::ErrorKind::UnexpectedEof.into()),
        _ => Ok(Response::InvalidRequest),
    } };
    tokio::spawn(open(Pending::new(request, reply, mounted, opening.clone())));
    let (edit, terminal) = tokio::select! {
        biased;
        incoming = stream.read(&mut extra) => {
            opening.cancel_pending();
            (incoming_error(incoming)?, None)
        },
        Ok(()) = &mut opened => (Response::Opened, None),
        _ = tokio::time::sleep_until(opening.deadline) => {
            let edit = if opening.cancel_pending() { Response::Expired }
                else if opened.await.is_ok() { Response::Opened }
                else { Response::AppExited };
            (edit, None)
        },
        reply = &mut result => {
            let reply = reply.unwrap_or(Response::AppExited);
            if opened.try_recv().is_ok() { (Response::Opened, Some(reply)) }
            else { (reply, None) }
        },
    };
    if edit != Response::Opened {
        opening.cancel.cancel();
    }
    send_edit(stream, edit, Instant::now() + DEADLINE).await?;
    if !wait || edit != Response::Opened {
        return Ok(());
    }
    let edit = match terminal {
        Some(reply) => reply,
        None => tokio::select! {
            biased;
            incoming = stream.read(&mut extra) => incoming_error(incoming)?,
            reply = result => reply.unwrap_or(Response::AppExited),
        },
    };
    send_edit(stream, edit, Instant::now() + DEADLINE).await
}

#[cfg(test)]
mod tests;

/// Owns a validated CLI request until the file is attached or opening ends.
#[must_use]
pub(crate) struct Pending {
    request: Request,
    reply: oneshot::Sender<Response>,
    opened: oneshot::Sender<()>,
    pub(crate) opening: Opening,
}

#[derive(Clone)]
pub(crate) struct Opening {
    pub(crate) cancel: tokio_util::sync::CancellationToken,
    pub(crate) deadline: Instant,
    phase: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl Opening {
    const PENDING: u8 = 0;
    const ATTACHED: u8 = 1;
    const CANCELLED: u8 = 2;

    pub(crate) fn new(deadline: Instant) -> Self {
        Self { cancel: tokio_util::sync::CancellationToken::new(), deadline, phase: Default::default() }
    }

    pub(crate) fn cancel_pending(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let won = self.phase.compare_exchange(Self::PENDING, Self::CANCELLED, Relaxed, Relaxed).is_ok();
        if won { self.cancel.cancel(); }
        won
    }

    fn attach(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        if self.is_inactive() { return false; }
        self.phase.compare_exchange(Self::PENDING, Self::ATTACHED, Relaxed, Relaxed).is_ok()
    }

    pub(crate) fn is_inactive(&self) -> bool {
        self.cancel.is_cancelled() || Instant::now() >= self.deadline
    }
}

impl Pending {
    pub(crate) fn new(request: Request, reply: oneshot::Sender<Response>, opened: oneshot::Sender<()>, opening: Opening) -> Self {
        Self { request, reply, opened, opening }
    }
    pub(crate) fn path(&self) -> &Path {
        &self.request.path
    }

    pub(crate) fn is_inactive(&self) -> bool {
        self.reply.is_closed() || self.opening.is_inactive()
    }

    pub(crate) fn open_failed(self) {
        let _ = self.reply.send(Response::OpenFailed);
    }

    /// Call after attaching the file to a new tab. Store the returned
    /// token in that exact tab's registry entry, never by pathname.
    pub(crate) fn opened(self) -> Result<WaitForClose, ()> {
        if !self.opening.attach() { return Err(()); }
        let _ = self.opened.send(());
        Ok(WaitForClose(self.reply))
    }
}

/// Dropping on app shutdown, navigation or window destruction is an error.
#[must_use]
pub(crate) struct WaitForClose(oneshot::Sender<Response>);

impl WaitForClose {
    /// Only the committed close of the tab that owns this token calls `closed`.
    pub(crate) fn closed(self) {
        let _ = self.0.send(Response::Closed);
    }
}
