use des::{
    Des,
    cipher::{Block, BlockEncrypt, KeyInit},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use std::{future::poll_fn, pin::Pin, task::Poll};
use zeroize::Zeroizing;

use super::{Auth, Error, IO_TIMEOUT, Rect, Secrets, Size, Target};

pub(super) const MAX_TEXT: usize = 1024 * 1024;
const MAX_PIXELS: usize = 8_294_400;
pub(super) const MAX_FRAME_BYTES: usize = MAX_PIXELS * 4;
const RAW: i32 = 0;
const COPY_RECT: i32 = 1;
const DESKTOP_SIZE: i32 = -223;
const EXTENDED_DESKTOP_SIZE: i32 = -308;

pub(super) async fn handshake(
    io: &mut (impl AsyncRead + AsyncWrite + Unpin),
    target: &Target,
    auth: Auth,
    secrets: &impl Secrets,
) -> Result<Size, Error> {
    use tokio::time::timeout;

    let (version, challenge) = timeout(IO_TIMEOUT, negotiate(io, auth)).await.map_err(|_| Error::Timeout)??;
    if let Some(challenge) = challenge {
        let secret = secrets.vnc_password(target).await.ok_or(Error::MissingSecret)?;
        timeout(IO_TIMEOUT, respond(io, secrets, secret, challenge)).await.map_err(|_| Error::Timeout)??;
    }
    timeout(IO_TIMEOUT, initialize(io, version, auth)).await.map_err(|_| Error::Timeout)?
}

async fn negotiate(
    io: &mut (impl AsyncRead + AsyncWrite + Unpin), auth: Auth,
) -> Result<([u8; 12], Option<Zeroizing<[u8; 16]>>), Error> {
    let mut version = [0; 12];
    io.read_exact(&mut version).await?;
    if !matches!(
        &version,
        b"RFB 003.003\n" | b"RFB 003.007\n" | b"RFB 003.008\n"
    ) {
        return Err(Error::UnsupportedVersion);
    }
    io.write_all(&version).await?;
    let security = match auth {
        Auth::VncPassword => 2,
        Auth::ExplicitNone => 1,
    };
    if &version == b"RFB 003.003\n" {
        if io.read_u32().await? != u32::from(security) {
            return Err(Error::SecurityType);
        }
    } else {
        let count = usize::from(io.read_u8().await?);
        let mut offered = [0; 255];
        io.read_exact(&mut offered[..count]).await?;
        if !offered[..count].contains(&security) {
            return Err(Error::SecurityType);
        }
        io.write_u8(security).await?;
    }
    let challenge = if auth == Auth::VncPassword {
        let mut challenge = Zeroizing::new([0; 16]);
        io.read_exact(challenge.as_mut()).await?;
        Some(challenge)
    } else { None };
    Ok((version, challenge))
}

async fn respond(
    io: &mut (impl AsyncWrite + Unpin), secrets: &impl Secrets,
    secret: Zeroizing<Vec<u8>>, mut challenge: Zeroizing<[u8; 16]>,
) -> Result<(), Error> {
    if !(1..=8).contains(&secret.len()) || secret.contains(&0) {
        return Err(Error::InvalidSecret);
    }
    {
        let mut key = Zeroizing::new([0_u8; 8]);
        for (dst, src) in key.iter_mut().zip(secret.iter()) {
            *dst = src.reverse_bits();
        }
        let cipher = Des::new_from_slice(key.as_slice()).map_err(|_| Error::InvalidSecret)?;
        for bytes in challenge.chunks_exact_mut(8) {
            cipher.encrypt_block(Block::<Des>::from_mut_slice(bytes));
        }
    }
    drop(secret);
    let mut remaining = challenge.as_slice();
    poll_fn(|cx| {
        while !remaining.is_empty() {
            let mut result = Poll::Pending;
            if !secrets.while_live(&mut || { result = Pin::new(&mut *io).poll_write(cx, remaining); }) {
                return Poll::Ready(Err(Error::Disconnected));
            }
            match result {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(0)) => return Poll::Ready(Err(Error::Disconnected)),
                Poll::Ready(Ok(written)) => remaining = &remaining[written..],
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error.into())),
            }
        }
        Poll::Ready(Ok(()))
    }).await
}

async fn initialize(
    io: &mut (impl AsyncRead + AsyncWrite + Unpin), version: [u8; 12], auth: Auth,
) -> Result<Size, Error> {
    if (auth == Auth::VncPassword || &version == b"RFB 003.008\n") && io.read_u32().await? != 0 {
        return Err(Error::Authentication);
    }
    io.write_u8(1).await?; // Shared ClientInit
    let size = read_size(io).await?;
    let mut server_format = [0; 16];
    io.read_exact(&mut server_format).await?;
    let _ = read_bytes(io, 4096).await?;
    // 32 bpp, 24-bit true color, little-endian, R/G/B shifts 0/8/16.
    io.write_all(&[
        0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0,
    ])
    .await?;
    io.write_all(&[2, 0, 0, 4]).await?;
    for encoding in [COPY_RECT, RAW, DESKTOP_SIZE, EXTENDED_DESKTOP_SIZE] {
        io.write_i32(encoding).await?;
    }
    request_frame(io, size, false).await?;
    Ok(size)
}

pub(super) async fn request_frame(
    io: &mut (impl AsyncWrite + Unpin),
    size: Size,
    incremental: bool,
) -> Result<(), Error> {
    let [w0, w1] = size.width.to_be_bytes();
    let [h0, h1] = size.height.to_be_bytes();
    io.write_all(&[3, u8::from(incremental), 0, 0, 0, 0, w0, w1, h0, h1])
        .await?;
    Ok(())
}

pub(super) async fn read_message(
    io: &mut (impl AsyncRead + Unpin),
    size: Size,
) -> Result<Message, Error> {
    match io.read_u8().await? {
        0 => {
            io.read_u8().await?;
            let count = io.read_u16().await?;
            if count > 4096 {
                return Err(Error::InvalidFrame);
            }
            let mut changes = Vec::new();
            let mut remaining = MAX_FRAME_BYTES;
            let mut resize_reply = false;
            let mut layout_batch = None;
            for index in 0..count {
                let rect = Rect {
                    x: io.read_u16().await?,
                    y: io.read_u16().await?,
                    size: read_pair(io).await?,
                };
                let encoding = io.read_i32().await?;
                let is_layout = encoding == EXTENDED_DESKTOP_SIZE;
                if *layout_batch.get_or_insert(is_layout) != is_layout {
                    return Err(Error::InvalidFrame);
                }
                match encoding {
                    RAW => {
                        rect.check(size)?;
                        let length = rect.size.pixels()? * 4;
                        remaining = remaining.checked_sub(length).ok_or(Error::InvalidFrame)?;
                        let mut rgba = vec![0; length];
                        io.read_exact(&mut rgba).await?;
                        for pixel in rgba.chunks_exact_mut(4) {
                            pixel[3] = 255;
                        }
                        changes.push(Update::Pixels(rect, rgba));
                    }
                    COPY_RECT => {
                        let source = Rect {
                            x: io.read_u16().await?,
                            y: io.read_u16().await?,
                            size: rect.size,
                        };
                        source.check(size)?;
                        rect.check(size)?;
                        changes.push(Update::Copy {
                            source,
                            destination: rect,
                        });
                    }
                    DESKTOP_SIZE => {
                        if rect.x != 0 || rect.y != 0 || index + 1 != count {
                            return Err(Error::InvalidFrame);
                        }
                        rect.size.pixels()?;
                        changes.push(Update::LegacySize(rect.size));
                    }
                    EXTENDED_DESKTOP_SIZE => {
                        let reason = rect.x;
                        let status = rect.y;
                        if reason > 2 {
                            return Err(Error::InvalidFrame);
                        }
                        let count = io.read_u8().await?;
                        let mut padding = [0; 3];
                        io.read_exact(&mut padding).await?;
                        let mut first = None;
                        let mut ids = std::collections::HashSet::new();
                        let applied = reason == 0 || (reason == 1 && status == 0);
                        if applied {
                            rect.size.pixels()?;
                        }
                        for _ in 0..count {
                            let id = io.read_u32().await?;
                            let bounds = Rect {
                                x: io.read_u16().await?,
                                y: io.read_u16().await?,
                                size: read_pair(io).await?,
                            };
                            let flags = io.read_u32().await?;
                            if applied {
                                bounds.check(rect.size)?;
                                if !ids.insert(id) {
                                    return Err(Error::InvalidFrame);
                                }
                                first = Some(Screen { id, bounds, flags });
                            }
                        }
                        if reason == 1 {
                            if resize_reply {
                                return Err(Error::InvalidFrame);
                            }
                            resize_reply = true;
                        }
                        if applied {
                            if count == 0 {
                                return Err(Error::InvalidFrame);
                            }
                        }
                        if reason != 2 {
                            changes.push(Update::Layout {
                                reply: (reason == 1).then_some(status),
                                applied: applied.then_some(rect.size),
                                single_screen: first.filter(|_| count == 1),
                            });
                        }
                    }
                    _ => return Err(Error::UnsupportedEncoding),
                }
            }
            Ok(Message::Frame(changes))
        }
        2 => Ok(Message::Bell),
        3 => {
            let mut padding = [0; 3];
            io.read_exact(&mut padding).await?;
            let bytes = read_bytes(io, MAX_TEXT).await?;
            Ok(Message::CutText(
                bytes.into_iter().map(char::from).collect(),
            ))
        }
        _ => Err(Error::UnsupportedMessage),
    }
}

pub(super) async fn request_resize(
    io: &mut (impl AsyncWrite + Unpin),
    screen: Screen,
) -> Result<(), Error> {
    let size = screen.bounds.size;
    let [w0, w1] = size.width.to_be_bytes();
    let [h0, h1] = size.height.to_be_bytes();
    let mut bytes = [
        251, 0, w0, w1, h0, h1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, w0, w1, h0, h1, 0, 0, 0, 0,
    ];
    bytes[8..12].copy_from_slice(&screen.id.to_be_bytes());
    bytes[20..24].copy_from_slice(&screen.flags.to_be_bytes());
    io.write_all(&bytes).await?;
    Ok(())
}

async fn read_pair(io: &mut (impl AsyncRead + Unpin)) -> Result<Size, Error> {
    Ok(Size {
        width: io.read_u16().await?,
        height: io.read_u16().await?,
    })
}

async fn read_size(io: &mut (impl AsyncRead + Unpin)) -> Result<Size, Error> {
    let size = read_pair(io).await?;
    size.pixels()?;
    Ok(size)
}

async fn read_bytes(io: &mut (impl AsyncRead + Unpin), cap: usize) -> Result<Vec<u8>, Error> {
    let count = io.read_u32().await? as usize;
    if count > cap {
        return Err(Error::LengthLimit);
    }
    let mut bytes = vec![0; count];
    io.read_exact(&mut bytes).await?;
    Ok(bytes)
}

impl Size {
    pub(super) fn pixels(self) -> Result<usize, Error> {
        let pixels = usize::from(self.width) * usize::from(self.height);
        if pixels == 0 || pixels > MAX_PIXELS || self.width > 8192 || self.height > 8192 {
            return Err(Error::InvalidFrame);
        }
        Ok(pixels)
    }
}

impl Rect {
    fn check(self, frame: Size) -> Result<(), Error> {
        self.size.pixels()?;
        if self
            .x
            .checked_add(self.size.width)
            .is_none_or(|end| end > frame.width)
            || self
                .y
                .checked_add(self.size.height)
                .is_none_or(|end| end > frame.height)
        {
            return Err(Error::InvalidFrame);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Screen {
    pub(super) id: u32,
    pub(super) bounds: Rect,
    pub(super) flags: u32,
}

pub(super) enum Message {
    Frame(Vec<Update>),
    Bell,
    CutText(String),
}

pub(super) enum Update {
    Pixels(Rect, Vec<u8>),
    Copy {
        source: Rect,
        destination: Rect,
    },
    LegacySize(Size),
    Layout {
        reply: Option<u16>,
        applied: Option<Size>,
        single_screen: Option<Screen>,
    },
}
