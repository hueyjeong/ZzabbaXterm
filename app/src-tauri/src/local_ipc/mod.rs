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
