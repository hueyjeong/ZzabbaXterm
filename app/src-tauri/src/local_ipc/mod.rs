use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
pub use unix::{Endpoint, Listener};
#[cfg(windows)]
pub use windows::{Endpoint, Listener};

pub const SERVICE: &str = "app";
pub const MAX_FRAME: usize = 16 * 1024;
pub const DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
const MAX_CONNECTIONS: usize = 16;

#[cfg(unix)]
type Stream = tokio::net::UnixStream;
#[cfg(windows)]
type Stream = tokio::net::windows::named_pipe::NamedPipeServer;

pub async fn serve<F>(mut listener: Listener, handle: impl Fn(Stream) -> F) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut clients = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = clients.join_next(), if !clients.is_empty() => {},
            accepted = listener.accept(), if clients.len() < MAX_CONNECTIONS => {
                match accepted {
                    Ok(stream) => { clients.spawn(handle(stream)); }
                    Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {},
                    Err(error) => return Err(error),
                }
            }
        }
    }
}

// F02는 접수 응답 후 별도 상태에서 편집을 진행한다.
pub fn encode_frame(value: &impl serde::Serialize, max: usize) -> serde_json::Result<Vec<u8>> {
    struct Bounded { bytes: Vec<u8>, max: usize }
    impl io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.max - self.bytes.len() {
                return Err(io::Error::new(io::ErrorKind::FileTooLarge, "IPC JSON exceeds the frame limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    let mut output = Bounded { bytes: Vec::new(), max };
    serde_json::to_writer(&mut output, value)?;
    Ok(output.bytes)
}

fn service_name(service: &str) -> io::Result<&str> {
    if service.is_empty()
        || !service
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid IPC service",
        ));
    }
    Ok(service)
}

pub async fn read_frame(stream: &mut (impl AsyncRead + Unpin), max: usize) -> io::Result<Vec<u8>> {
    let length = stream.read_u32().await? as usize;
    if length > max {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "IPC frame is too large",
        ));
    }
    if length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid IPC frame length",
        ));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

pub async fn write_frame(
    stream: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
    max: usize,
) -> io::Result<()> {
    let length = u32::try_from(bytes.len())
        .ok()
        .filter(|&len| len > 0 && bytes.len() <= max)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid IPC frame length"))?;
    stream.write_u32(length).await?;
    stream.write_all(bytes).await
}
