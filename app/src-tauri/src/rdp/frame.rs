use ironrdp::{
    core::{ReadCursor, decode_cursor},
    pdu::{
        Action,
        bitmap::{BitmapData, BitmapUpdateData, Compression},
        fast_path::{FastPathHeader, FastPathUpdatePdu, Fragmentation, UpdateCode},
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
        if compression_flags.contains(Compression::BITMAP_COMPRESSION) {
            // IronRDP decodes bitmap source dimensions but applies rows using the destination width.
            if row != draw_row {
                return Err(Error::Bounds);
            }
        } else if bitmap_data.len() != ((row + 3) & !3) * usize::from(height) || row != draw_row {
            return Err(Error::Bounds);
        }
    }
    Ok(has_rects)
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
