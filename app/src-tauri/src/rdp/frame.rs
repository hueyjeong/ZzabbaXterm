use ironrdp::{
    core::{ReadCursor, decode_cursor, encode_vec},
    graphics::rdp6::BitmapStreamDecoder,
    pdu::{
        Action,
        bitmap::{BitmapData, BitmapUpdateData, Compression},
        fast_path::{EncryptionFlags, FastPathHeader, FastPathUpdatePdu, Fragmentation, UpdateCode},
        geometry::InclusiveRectangle,
        mcs::decode_send_data_indication,
        rdp::{
            headers::{IoChannelPdu, ShareDataPdu, decode_io_channel},
            vc::{ChannelControlFlags as Flags, ChannelPduHeader},
        },
        slow_path::{self, GraphicsUpdateType},
        surface_commands::SurfaceCommand,
    },
    session::image::DecodedImage,
};

use ironrdp_tokio::bytes::Bytes;

use super::{Error, SessionId};

const MAX_FRAME: usize = 3840 * 2160 * 4;
const MAX_CHANNEL: u32 = 256 * 1024;

pub(super) fn frame_len(width: u16, height: u16) -> Result<usize, Error> {
    if width > 4096 || height > 4096 {
        return Err(Error::Bounds);
    }
    let len = usize::from(width) * usize::from(height) * 4;
    if len == 0 || len > MAX_FRAME {
        return Err(Error::Bounds);
    }
    Ok(len)
}

#[derive(Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

pub struct Frame {
    pub session: SessionId,
    pub width: u16,
    pub height: u16,
    pub rect: Rect,
    /// Tightly packed RGBA8 pixels, top-down, with no row padding.
    pub rgba: Vec<u8>,
}

pub(super) fn patch(
    id: SessionId,
    image: &DecodedImage,
    dirty: &InclusiveRectangle,
    full: bool,
) -> Result<Frame, Error> {
    check_rect(image, dirty)?;
    let (width, height) = (image.width(), image.height());
    let rect = if full {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    } else {
        Rect {
            x: dirty.left,
            y: dirty.top,
            width: dirty.right - dirty.left + 1,
            height: dirty.bottom - dirty.top + 1,
        }
    };
    let stride = usize::from(width) * 4;
    let row_len = usize::from(rect.width) * 4;
    let x = usize::from(rect.x) * 4;
    let start = usize::from(rect.y);
    let mut rgba = Vec::with_capacity(row_len * usize::from(rect.height));
    for y in start..start + usize::from(rect.height) {
        rgba.extend_from_slice(&image.data()[y * stride + x..y * stride + x + row_len]);
    }
    // Some RDP bitmap encodings carry no alpha; the desktop is opaque.
    for alpha in rgba.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }
    Ok(Frame {
        session: id,
        width,
        height,
        rect,
        rgba,
    })
}

fn check_rect(image: &DecodedImage, rect: &InclusiveRectangle) -> Result<(), Error> {
    let (width, height) = (image.width(), image.height());
    if image.data().len() != frame_len(width, height)?
        || image.bytes_per_pixel() != 4
        || rect.left > rect.right
        || rect.top > rect.bottom
        || rect.right >= width
        || rect.bottom >= height
    {
        return Err(Error::Bounds);
    }
    Ok(())
}

pub(super) fn has_pixels(image: &DecodedImage, rect: &InclusiveRectangle) -> Result<bool, Error> {
    check_rect(image, rect)?;
    let stride = usize::from(image.width()) * 4;
    let start = usize::from(rect.left) * 4;
    let end = (usize::from(rect.right) + 1) * 4;
    Ok(
        (usize::from(rect.top)..=usize::from(rect.bottom)).any(|row| {
            image.data()[row * stride + start..row * stride + end]
                .iter()
                .skip(3)
                .step_by(4)
                .any(|alpha| *alpha == 255)
        }),
    )
}

#[derive(Default)]
pub(super) struct Inbound {
    pub(super) io_id: u16,
    pub(super) clipboard_id: Option<u16>,
    pub(super) display_carrier_id: Option<u16>,
    clipboard: Chunk,
    display: Chunk,
    incomplete_graphics: Option<(UpdateCode, Vec<u8>)>,
}

impl Inbound {
    pub(super) fn check(
        &mut self,
        action: Action,
        pdu: &[u8],
        image: &DecodedImage,
    ) -> Result<bool, Error> {
        if pdu.is_empty() || pdu.len() > 65_535 {
            return Err(Error::Bounds);
        }
        let mut pixels = false;
        match action {
            Action::FastPath => {
                let mut input = ReadCursor::new(pdu);
                let header: FastPathHeader =
                    decode_cursor(&mut input).map_err(|_| Error::Protocol)?;
                if header.data_length != input.remaining().len() || !header.flags.is_empty() {
                    return Err(Error::Protocol);
                }
                while !input.is_empty() {
                    let update: FastPathUpdatePdu<'_> =
                        decode_cursor(&mut input).map_err(|_| Error::Protocol)?;
                    if update.compression_flags.is_some() {
                        return Err(Error::Protocol);
                    }
                    match update.fragmentation {
                        Fragmentation::Single => {
                            self.incomplete_graphics = None;
                            pixels |= check_graphics(update.update_code, update.data, image)?;
                        }
                        Fragmentation::First => {
                            self.incomplete_graphics =
                                Some((update.update_code, update.data.to_vec()))
                        }
                        Fragmentation::Next | Fragmentation::Last => {
                            let (code, buffer) =
                                self.incomplete_graphics.as_mut().ok_or(Error::Protocol)?;
                            if *code != update.update_code {
                                return Err(Error::Protocol);
                            }
                            if buffer.len() + update.data.len() > MAX_FRAME {
                                return Err(Error::Bounds);
                            }
                            buffer.extend_from_slice(update.data);
                            if update.fragmentation == Fragmentation::Last {
                                pixels |= check_graphics(*code, buffer, image)?;
                                self.incomplete_graphics = None;
                            }
                        }
                    }
                }
            }
            Action::X224 => {
                // Disconnect PDUs also use X.224; leave those to IronRDP.
                if let Ok(data) = decode_send_data_indication(pdu) {
                    let id = Some(data.channel_id);
                    if data.channel_id == self.io_id {
                        if let IoChannelPdu::Data(ctx) =
                            decode_io_channel(data).map_err(|_| Error::Protocol)?
                        {
                            if let ShareDataPdu::Update(bytes) = ctx.pdu {
                                let mut input = ReadCursor::new(&bytes);
                                if slow_path::read_graphics_update_type(&mut input)
                                    .map_err(|_| Error::Protocol)?
                                    == GraphicsUpdateType::Bitmap
                                {
                                    let bitmap = slow_path::decode_slow_path_bitmap(&mut input)
                                        .map_err(|_| Error::Protocol)?;
                                    pixels |= check_bitmaps(bitmap, image)?;
                                }
                            }
                        }
                    } else if id == self.clipboard_id {
                        self.clipboard.check(data.user_data, MAX_CHANNEL)?;
                    } else if id == self.display_carrier_id {
                        // Display Control capabilities and the carrier's control PDUs fit in one small message.
                        self.display.check(data.user_data, 1024)?;
                        if self.display.pending.is_some() {
                            return Err(Error::Protocol);
                        }
                        let mut payload = ReadCursor::new(data.user_data);
                        let _: ChannelPduHeader =
                            decode_cursor(&mut payload).map_err(|_| Error::Protocol)?;
                        // The only registered DVC receives a single capabilities PDU, not DVC fragments.
                        if payload
                            .remaining()
                            .first()
                            .is_some_and(|header| header >> 4 == 2)
                        {
                            return Err(Error::Protocol);
                        }
                    }
                }
            }
        }
        Ok(pixels)
    }

    pub(super) fn reset_graphics(&mut self) {
        self.incomplete_graphics = None;
    }
}

fn check_graphics(code: UpdateCode, data: &[u8], image: &DecodedImage) -> Result<bool, Error> {
    let mut input = ReadCursor::new(data);
    match code {
        UpdateCode::Bitmap => check_bitmaps(
            decode_cursor(&mut input).map_err(|_| Error::Protocol)?,
            image,
        ),
        UpdateCode::SurfaceCommands => {
            let mut pixels = false;
            while !input.is_empty() {
                match decode_cursor(&mut input).map_err(|_| Error::Protocol)? {
                    SurfaceCommand::SetSurfaceBits(bits)
                    | SurfaceCommand::StreamSurfaceBits(bits) => {
                        let dest = bits.destination;
                        let bitmap = bits.extended_bitmap_data;
                        let bounds = dest.left < dest.right
                            && dest.top < dest.bottom
                            && dest.right <= image.width()
                            && dest.bottom <= image.height();
                        if !bounds
                            || bitmap.codec_id != 0
                            || bitmap.width != dest.right - dest.left
                            || bitmap.height != dest.bottom - dest.top
                        {
                            return Err(Error::Bounds);
                        }
                        if bitmap.data.len()
                            != row_bytes(bitmap.width, u16::from(bitmap.bpp))?
                                * usize::from(bitmap.height)
                        {
                            return Err(Error::Bounds);
                        }
                        pixels = true;
                    }
                    SurfaceCommand::FrameMarker(_) => {}
                }
            }
            Ok(pixels)
        }
        _ => Ok(false),
    }
}

fn row_bytes(width: u16, bpp: u16) -> Result<usize, Error> {
    if !matches!(bpp, 8 | 15 | 16 | 24 | 32) {
        return Err(Error::Protocol);
    }
    Ok(usize::from(width) * usize::from(bpp.div_ceil(8)))
}

fn check_bitmaps(bitmap: BitmapUpdateData<'_>, image: &DecodedImage) -> Result<bool, Error> {
    let has_rects = !bitmap.rectangles.is_empty();
    for BitmapData {
        rectangle: rect,
        width,
        height,
        bits_per_pixel,
        compression_flags,
        bitmap_data,
        ..
    } in bitmap.rectangles
    {
        if rect.left > rect.right
            || rect.top > rect.bottom
            || rect.right >= image.width()
            || rect.bottom >= image.height()
            || rect.bottom - rect.top + 1 != height
            || width < rect.right - rect.left + 1
        {
            return Err(Error::Bounds);
        }
        frame_len(width, height)?;
        let row = row_bytes(width, bits_per_pixel)?;
        let draw_row = row_bytes(rect.right - rect.left + 1, bits_per_pixel)?;
        // normalize_bitmap이 압축 비트맵을 목적지 폭으로 잘라 낸 뒤에만 이 검사에 도달한다.
        if compression_flags.contains(Compression::BITMAP_COMPRESSION) {
            if row != draw_row {
                return Err(Error::Bounds);
            }
        } else if bitmap_data.len() != ((row + 3) & !3) * usize::from(height) || row != draw_row {
            return Err(Error::Bounds);
        }
    }
    Ok(has_rects)
}

struct Source {
    rect: InclusiveRectangle,
    width: u16,
    height: u16,
    bits_per_pixel: u16,
    compressed: bool,
    data: Vec<u8>,
}

fn sources(bitmap: &BitmapUpdateData<'_>) -> Vec<Source> {
    bitmap.rectangles.iter().map(|update| Source {
        rect: update.rectangle.clone(),
        width: update.width,
        height: update.height,
        bits_per_pixel: update.bits_per_pixel,
        compressed: update.compression_flags.contains(Compression::BITMAP_COMPRESSION),
        data: update.bitmap_data.to_vec(),
    }).collect()
}

fn fastpath_bitmap(pdu: &[u8]) -> Result<Option<Vec<Source>>, Error> {
    let mut input = ReadCursor::new(pdu);
    let header: FastPathHeader = decode_cursor(&mut input).map_err(|_| Error::Protocol)?;
    if header.data_length != input.remaining().len() || !header.flags.is_empty() {
        return Err(Error::Protocol);
    }
    let mut found = None;
    while !input.is_empty() {
        let update: FastPathUpdatePdu<'_> = decode_cursor(&mut input).map_err(|_| Error::Protocol)?;
        if update.compression_flags.is_some() || update.fragmentation != Fragmentation::Single {
            return Ok(None);
        }
        if update.update_code == UpdateCode::Bitmap {
            let bitmap: BitmapUpdateData<'_> = decode_cursor(&mut ReadCursor::new(update.data))
                .map_err(|_| Error::Protocol)?;
            if found.is_some() {
                return Ok(None);
            }
            found = Some(sources(&bitmap));
        }
    }
    Ok(found)
}

fn slowpath_bitmap(pdu: &[u8]) -> Result<Option<Vec<Source>>, Error> {
    let Ok(data) = decode_send_data_indication(pdu) else { return Ok(None) };
    let Ok(IoChannelPdu::Data(ctx)) = decode_io_channel(data).map_err(|_| Error::Protocol) else { return Ok(None) };
    let ShareDataPdu::Update(bytes) = ctx.pdu else { return Ok(None) };
    let mut input = ReadCursor::new(&bytes);
    if slow_path::read_graphics_update_type(&mut input).map_err(|_| Error::Protocol)? != GraphicsUpdateType::Bitmap {
        return Ok(None);
    }
    let bitmap = slow_path::decode_slow_path_bitmap(&mut input).map_err(|_| Error::Protocol)?;
    Ok(Some(sources(&bitmap)))
}

fn crop_bitmap(update: &Source) -> Result<(u16, Vec<u8>), Error> {
    let src = usize::from(update.width);
    let dest_width = update.rect.right.saturating_sub(update.rect.left).saturating_add(1);
    let dest = usize::from(dest_width);
    let height = usize::from(update.height);
    if src < dest || height == 0 {
        return Err(Error::Bounds);
    }
    if update.compressed {
        // 관측된 xrdp RDP6(32bpp)만 원본 폭으로 디코드한 뒤 목적지 폭으로 잘라 낸다.
        if update.bits_per_pixel != 32 {
            return Err(Error::Bounds);
        }
        let mut rgb = Vec::new();
        BitmapStreamDecoder::default()
            .decode_bitmap_stream_to_rgb24(&update.data, &mut rgb, src, height)
            .map_err(|_| Error::Protocol)?;
        let mut out = Vec::with_capacity(dest * height * 4);
        for row in rgb.chunks_exact(src * 3) {
            for pixel in row[..dest * 3].chunks_exact(3) {
                out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0]);
            }
        }
        return Ok((32, out));
    }
    let bytes = usize::from(update.bits_per_pixel.div_ceil(8));
    let source_row = (src * bytes + 3) & !3;
    let destination_row = (dest * bytes + 3) & !3;
    if update.data.len() < source_row * height {
        return Err(Error::Bounds);
    }
    let mut out = vec![0; destination_row * height];
    for (index, row) in update.data.chunks_exact(source_row).take(height).enumerate() {
        out[index * destination_row..index * destination_row + dest * bytes]
            .copy_from_slice(&row[..dest * bytes]);
    }
    Ok((update.bits_per_pixel, out))
}

struct Cropped {
    rect: InclusiveRectangle,
    height: u16,
    bits_per_pixel: u16,
    data: Vec<u8>,
}

/// 압축 비트맵을 원본 폭으로 디코드해 목적지 폭으로 잘라 낸 새 uncompressed fast-path PDU를 만든다.
pub(super) fn normalize_bitmap(action: Action, pdu: &[u8]) -> Result<Option<(Action, Bytes)>, Error> {
    let sources = match action {
        Action::FastPath => fastpath_bitmap(pdu)?,
        Action::X224 => slowpath_bitmap(pdu)?,
        _ => return Ok(None),
    };
    let Some(sources) = sources else { return Ok(None) };
    if !sources.iter().any(|update| update.width > update.rect.right - update.rect.left + 1) {
        return Ok(None);
    }
    let mut cropped = Vec::with_capacity(sources.len());
    for update in &sources {
        let (bits_per_pixel, data) = crop_bitmap(update)?;
        cropped.push(Cropped {
            rect: update.rect.clone(),
            height: update.height,
            bits_per_pixel,
            data,
        });
    }
    let rectangles = cropped.iter().map(|update| BitmapData {
        rectangle: update.rect.clone(),
        width: update.rect.right - update.rect.left + 1,
        height: update.height,
        bits_per_pixel: update.bits_per_pixel,
        compression_flags: Compression::empty(),
        compressed_data_header: None,
        bitmap_data: &update.data,
    }).collect::<Vec<_>>();
    let data = encode_vec(&BitmapUpdateData { rectangles }).map_err(|_| Error::Protocol)?;
    let update = encode_vec(&FastPathUpdatePdu {
        fragmentation: Fragmentation::Single,
        update_code: UpdateCode::Bitmap,
        compression_flags: None,
        compression_type: None,
        data: &data,
    }).map_err(|_| Error::Protocol)?;
    let mut output = encode_vec(&FastPathHeader::new(EncryptionFlags::empty(), update.len()))
        .map_err(|_| Error::Protocol)?;
    output.extend_from_slice(&update);
    Ok(Some((Action::FastPath, Bytes::from(output))))
}

#[derive(Default)]
struct Chunk {
    pending: Option<(u32, u32)>,
}

impl Chunk {
    fn check(&mut self, data: &[u8], limit: u32) -> Result<(), Error> {
        let mut payload = ReadCursor::new(data);
        let header: ChannelPduHeader = decode_cursor(&mut payload).map_err(|_| Error::Protocol)?;
        if header.length > limit {
            return Err(Error::Bounds);
        }
        if header
            .flags
            .intersects(Flags::PACKET_COMPRESSED | Flags::PACKET_AT_FRONT | Flags::PACKET_FLUSHED)
        {
            return Err(Error::Protocol);
        }
        let offset = match (self.pending, header.flags.contains(Flags::FLAG_FIRST)) {
            (None, true) => 0,
            (Some((len, offset)), false) if len == header.length => offset,
            _ => return Err(Error::Protocol),
        };
        let total = offset + u32::try_from(payload.remaining().len()).map_err(|_| Error::Bounds)?;
        let last = header.flags.contains(Flags::FLAG_LAST);
        if total > header.length || last != (total == header.length) {
            return Err(Error::Bounds);
        }
        self.pending = (!last).then_some((header.length, total));
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests;
