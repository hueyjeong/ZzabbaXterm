use ironrdp::{
    core::{ReadCursor, decode_cursor, encode_vec},
    graphics::rdp6::BitmapStreamDecoder,
    pdu::{
        Action,
        bitmap::{BitmapData, BitmapUpdateData, Compression},
        fast_path::{EncryptionFlags, FastPathHeader, FastPathUpdatePdu, Fragmentation, UpdateCode},
        geometry::InclusiveRectangle,
        mcs::{SendDataIndication, SendDataIndicationCtx, decode_send_data_indication},
        rdp::{
            headers::{IoChannelPdu, ShareDataPdu, decode_io_channel},
            vc::{ChannelControlFlags as Flags, ChannelPduHeader},
        },
        slow_path::{self, GraphicsUpdateType},
        surface_commands::SurfaceCommand,
        x224::X224,
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

pub(super) enum Normalized {
    Pass,
    Drop,
    Replace(Vec<(Action, Bytes)>),
}

#[derive(Default)]
pub(super) struct Normalizer {
    fragment: Option<Vec<u8>>,
}

fn extend_fragment(buffer: &mut Vec<u8>, data: &[u8], limit: usize) -> Result<(), Error> {
    if buffer.len() + data.len() > limit {
        return Err(Error::Bounds);
    }
    buffer.extend_from_slice(data);
    Ok(())
}

fn validate(bitmap: &BitmapUpdateData<'_>, image: &DecodedImage) -> Result<(), Error> {
    for update in &bitmap.rectangles {
        let rect = &update.rectangle;
        let width = rect.right - rect.left + 1;
        if rect.left > rect.right
            || rect.top > rect.bottom
            || rect.right >= image.width()
            || rect.bottom >= image.height()
            || update.height == 0
            || rect.bottom - rect.top + 1 != update.height
            || update.width < width
        {
            return Err(Error::Bounds);
        }
        // 원격이 선언한 크기로 디코드하기 전에 기존 프레임 한도를 먼저 적용한다.
        frame_len(update.width, update.height)?;
        if !update.compression_flags.contains(Compression::BITMAP_COMPRESSION) {
            let row = row_bytes(update.width, update.bits_per_pixel).map_err(|_| Error::Bounds)?;
            let row = (row + 3) & !3;
            if update.bitmap_data.len() < row * usize::from(update.height) {
                return Err(Error::Bounds);
            }
        }
    }
    Ok(())
}

fn needs_crop(bitmap: &BitmapUpdateData<'_>) -> bool {
    bitmap.rectangles.iter().any(|update| update.width > update.rectangle.right - update.rectangle.left + 1)
}

fn decode_rect(update: &BitmapData<'_>) -> Result<(u16, Vec<u8>), Error> {
    let source = usize::from(update.width);
    let dest = usize::from(update.rectangle.right - update.rectangle.left + 1);
    let height = usize::from(update.height);
    if source < dest || height == 0 {
        return Err(Error::Bounds);
    }
    if update.compression_flags.contains(Compression::BITMAP_COMPRESSION) {
        // 잘라 내야 하는 압축 비트맵은 RDP6(32bpp)만 디코드한다. 그 밖의 압축은 IronRDP가 그대로 처리한다.
        if update.bits_per_pixel != 32 {
            return Err(Error::Bounds);
        }
        // 압축은 원본 폭으로 디코드한 뒤 목적지 폭으로 잘라 낸다(행 밀림 방지).
        let mut rgb = Vec::new();
        BitmapStreamDecoder::default()
            .decode_bitmap_stream_to_rgb24(update.bitmap_data, &mut rgb, source, height)
            .map_err(|_| Error::Protocol)?;
        // 원격 스트림이 선언한 행 수보다 짧으면 잘라 내기 전에 거부한다.
        if rgb.len() != source * height * 3 {
            return Err(Error::Bounds);
        }
        let mut out = Vec::with_capacity(dest * height * 4);
        for row in rgb.chunks_exact(source * 3) {
            for pixel in row[..dest * 3].chunks_exact(3) {
                out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0]);
            }
        }
        return Ok((32, out));
    }
    let bytes = usize::from(update.bits_per_pixel.div_ceil(8));
    let source_row = (source * bytes + 3) & !3;
    let destination_row = (dest * bytes + 3) & !3;
    let mut out = vec![0; destination_row * height];
    for (index, row) in update.bitmap_data.chunks_exact(source_row).take(height).enumerate() {
        out[index * destination_row..index * destination_row + dest * bytes]
            .copy_from_slice(&row[..dest * bytes]);
    }
    Ok((update.bits_per_pixel, out))
}

fn bitmap_pdu(rect: InclusiveRectangle, width: u16, height: u16, bits_per_pixel: u16, data: &[u8]) -> Result<Bytes, Error> {
    let encoded = encode_vec(&BitmapUpdateData {
        rectangles: vec![BitmapData {
            rectangle: rect,
            width,
            height,
            bits_per_pixel,
            compression_flags: Compression::empty(),
            compressed_data_header: None,
            bitmap_data: data,
        }],
    }).map_err(|_| Error::Protocol)?;
    let update = encode_vec(&FastPathUpdatePdu {
        fragmentation: Fragmentation::Single,
        update_code: UpdateCode::Bitmap,
        compression_flags: None,
        compression_type: None,
        data: &encoded,
    }).map_err(|_| Error::Protocol)?;
    let mut output = encode_vec(&FastPathHeader::new(EncryptionFlags::empty(), update.len()))
        .map_err(|_| Error::Protocol)?;
    output.extend_from_slice(&update);
    Ok(Bytes::from(output))
}

fn bitmap_pdus(bitmap: &BitmapUpdateData<'_>, image: &DecodedImage, force: bool) -> Result<Vec<Bytes>, Error> {
    validate(bitmap, image)?;
    if !force && !needs_crop(bitmap) {
        return Ok(Vec::new());
    }
    let mut output = Vec::new();
    for update in &bitmap.rectangles {
        let (bits_per_pixel, data) = decode_rect(update)?;
        let dest_width = update.rectangle.right - update.rectangle.left + 1;
        let bytes = usize::from(bits_per_pixel.div_ceil(8));
        let row = (usize::from(dest_width) * bytes + 3) & !3;
        let height = usize::from(update.height);
        let mut start = 0;
        while start < height {
            let mut rows = ((32_767 - 256) / row.max(1)).clamp(1, height - start);
            let pdu = loop {
                let top = update.rectangle.top + u16::try_from(height - start - rows).map_err(|_| Error::Bounds)?;
                let bottom = update.rectangle.top + u16::try_from(height - start - 1).map_err(|_| Error::Bounds)?;
                let slice = &data[start * row..(start + rows) * row];
                let pdu = bitmap_pdu(
                    InclusiveRectangle { left: update.rectangle.left, top, right: update.rectangle.right, bottom },
                    dest_width, u16::try_from(rows).map_err(|_| Error::Bounds)?, bits_per_pixel, slice,
                )?;
                if pdu.len() <= 32_767 || rows == 1 { break pdu; }
                rows -= 1;
            };
            output.push(pdu);
            start += rows;
        }
    }
    Ok(output)
}

fn updates_pdu(updates: &[(Fragmentation, UpdateCode, Vec<u8>)]) -> Result<Bytes, Error> {
    let mut payload = Vec::new();
    for (fragmentation, code, data) in updates {
        let encoded = encode_vec(&FastPathUpdatePdu {
            fragmentation: *fragmentation,
            update_code: *code,
            compression_flags: None,
            compression_type: None,
            data,
        }).map_err(|_| Error::Protocol)?;
        payload.extend_from_slice(&encoded);
    }
    let mut output = encode_vec(&FastPathHeader::new(EncryptionFlags::empty(), payload.len()))
        .map_err(|_| Error::Protocol)?;
    output.extend_from_slice(&payload);
    Ok(Bytes::from(output))
}

impl Normalizer {
    pub(super) fn normalize(&mut self, action: Action, pdu: &[u8], image: &DecodedImage) -> Result<Normalized, Error> {
        match action {
            Action::FastPath => self.fastpath(pdu, image),
            Action::X224 => self.slowpath(pdu, image),
            _ => Ok(Normalized::Pass),
        }
    }

    fn fastpath(&mut self, pdu: &[u8], image: &DecodedImage) -> Result<Normalized, Error> {
        let mut input = ReadCursor::new(pdu);
        let header: FastPathHeader = decode_cursor(&mut input).map_err(|_| Error::Protocol)?;
        if header.data_length != input.remaining().len() || !header.flags.is_empty() {
            return Err(Error::Protocol);
        }
        let mut segment: Vec<(Fragmentation, UpdateCode, Vec<u8>)> = Vec::new();
        let mut output: Vec<Bytes> = Vec::new();
        let mut changed = false;
        let mut consumed_fragment = false;
        while !input.is_empty() {
            let update: FastPathUpdatePdu<'_> = decode_cursor(&mut input).map_err(|_| Error::Protocol)?;
            if update.compression_flags.is_some() {
                return Err(Error::Protocol);
            }
            if update.update_code != UpdateCode::Bitmap {
                segment.push((update.fragmentation, update.update_code, update.data.to_vec()));
                continue;
            }
            match update.fragmentation {
                Fragmentation::Single => {
                    self.fragment = None;
                    let bitmap: BitmapUpdateData<'_> = decode_cursor(&mut ReadCursor::new(update.data))
                        .map_err(|_| Error::Protocol)?;
                    validate(&bitmap, image)?;
                    if needs_crop(&bitmap) {
                        changed = true;
                        if !segment.is_empty() {
                            output.push(updates_pdu(&segment)?);
                            segment.clear();
                        }
                        output.extend(bitmap_pdus(&bitmap, image, false)?);
                    } else {
                        segment.push((Fragmentation::Single, UpdateCode::Bitmap, update.data.to_vec()));
                    }
                }
                Fragmentation::First => {
                    self.fragment = Some(update.data.to_vec());
                    consumed_fragment = true;
                }
                Fragmentation::Next | Fragmentation::Last => {
                    consumed_fragment = true;
                    let mut buffer = self.fragment.take().ok_or(Error::Protocol)?;
                    extend_fragment(&mut buffer, update.data, MAX_FRAME)?;
                    if update.fragmentation == Fragmentation::Last {
                        changed = true;
                        let bitmap: BitmapUpdateData<'_> = decode_cursor(&mut ReadCursor::new(&buffer))
                            .map_err(|_| Error::Protocol)?;
                        if !segment.is_empty() {
                            output.push(updates_pdu(&segment)?);
                            segment.clear();
                        }
                        output.extend(bitmap_pdus(&bitmap, image, true)?);
                    } else {
                        self.fragment = Some(buffer);
                    }
                }
            }
        }
        if !changed && !consumed_fragment {
            return Ok(Normalized::Pass);
        }
        if !segment.is_empty() {
            output.push(updates_pdu(&segment)?);
        }
        // 비트맵 조각만 담긴 PDU는 재조립이 끝날 때까지 보낼 것이 없다.
        if output.is_empty() {
            return Ok(Normalized::Drop);
        }
        Ok(Normalized::Replace(output.into_iter().map(|pdu| (Action::FastPath, pdu)).collect()))
    }

    fn slowpath(&mut self, pdu: &[u8], image: &DecodedImage) -> Result<Normalized, Error> {
        let Ok(data) = decode_send_data_indication(pdu) else { return Ok(Normalized::Pass) };
        if let Some(pdu) = full_deactivate(&data)? {
            return Ok(Normalized::Replace(vec![(Action::X224, pdu)]));
        }
        let Ok(IoChannelPdu::Data(ctx)) = decode_io_channel(data).map_err(|_| Error::Protocol) else { return Ok(Normalized::Pass) };
        let ShareDataPdu::Update(bytes) = ctx.pdu else { return Ok(Normalized::Pass) };
        let mut input = ReadCursor::new(&bytes);
        if slow_path::read_graphics_update_type(&mut input).map_err(|_| Error::Protocol)? != GraphicsUpdateType::Bitmap {
            return Ok(Normalized::Pass);
        }
        let bitmap = slow_path::decode_slow_path_bitmap(&mut input).map_err(|_| Error::Protocol)?;
        validate(&bitmap, image)?;
        // 비트맵 뒤에 다른 데이터가 붙은 Update PDU는 바꾸지 않고 원본대로 처리한다.
        if !input.is_empty() {
            return Ok(Normalized::Pass);
        }
        let pdus = bitmap_pdus(&bitmap, image, false)?;
        if pdus.is_empty() {
            Ok(Normalized::Pass)
        } else {
            Ok(Normalized::Replace(pdus.into_iter().map(|pdu| (Action::FastPath, pdu)).collect()))
        }
    }
}

/// xrdp 0.9.24는 해상도를 바꿀 때 Deactivate All을 ShareControlHeader 6바이트만으로 보낸다.
/// IronRDP는 shareId까지 읽어야 하므로 shareId 0과 1바이트 sourceDescriptor를 붙인 정상 PDU로 바꿔 재활성화가 이어지게 한다.
fn full_deactivate(data: &SendDataIndicationCtx<'_>) -> Result<Option<Bytes>, Error> {
    const DEACTIVATE_ALL: u16 = 0x16;
    let user = data.user_data;
    if user.len() != 6 || user[..2] != [6, 0] || u16::from_le_bytes([user[2], user[3]]) != DEACTIVATE_ALL {
        return Ok(None);
    }
    let mut fixed = Vec::with_capacity(13);
    fixed.extend_from_slice(&13_u16.to_le_bytes());
    fixed.extend_from_slice(&user[2..6]);
    fixed.extend_from_slice(&0_u32.to_le_bytes());
    fixed.extend_from_slice(&1_u16.to_le_bytes());
    fixed.push(0);
    let pdu = encode_vec(&X224(SendDataIndication {
        initiator_id: data.initiator_id,
        channel_id: data.channel_id,
        user_data: std::borrow::Cow::Owned(fixed),
    }))
    .map_err(|_| Error::Protocol)?;
    Ok(Some(Bytes::from(pdu)))
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
