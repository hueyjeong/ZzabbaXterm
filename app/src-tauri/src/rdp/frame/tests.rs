use ironrdp::{
    core::encode_vec,
    graphics::image_processing::PixelFormat,
    pdu::fast_path::EncryptionFlags,
    session::{ActiveStageBuilder, ActiveStageOutput},
};

use super::*;

pub(in crate::rdp) fn bitmap_frame(rectangle: InclusiveRectangle) -> Vec<u8> {
    encode_bitmap(BitmapUpdateData {
        rectangles: vec![BitmapData {
            rectangle,
            width: 1,
            height: 2,
            bits_per_pixel: 24,
            compression_flags: Compression::empty(),
            compressed_data_header: None,
            bitmap_data: &[255, 0, 0, 0, 0, 0, 255, 0],
        }],
    })
}

pub(in crate::rdp) fn corrupt_bitmap_frame(rectangle: InclusiveRectangle) -> Vec<u8> {
    encode_bitmap(BitmapUpdateData {
        rectangles: vec![BitmapData {
            rectangle,
            width: 1,
            height: 2,
            bits_per_pixel: 32,
            compression_flags: Compression::BITMAP_COMPRESSION
                | Compression::NO_BITMAP_COMPRESSION_HDR,
            compressed_data_header: None,
            bitmap_data: &[],
        }],
    })
}

fn encode_bitmap(bitmap: BitmapUpdateData<'_>) -> Vec<u8> {
    fastpath(UpdateCode::Bitmap, &encode_vec(&bitmap).unwrap())
}

pub(in crate::rdp) fn marker_frame(end: bool) -> Vec<u8> {
    use ironrdp::pdu::surface_commands::{FrameAction, FrameMarkerPdu};
    let marker = SurfaceCommand::FrameMarker(FrameMarkerPdu {
        frame_action: if end {
            FrameAction::End
        } else {
            FrameAction::Begin
        },
        frame_id: Some(7),
    });
    fastpath(UpdateCode::SurfaceCommands, &encode_vec(&marker).unwrap())
}

pub(in crate::rdp) fn marked_bitmap_frame() -> Vec<u8> {
    let marker = marker_frame(true);
    let bitmap = bitmap_frame(InclusiveRectangle {
        left: 1,
        top: 0,
        right: 1,
        bottom: 1,
    });
    let mut payload = marker[2..].to_vec();
    payload.extend_from_slice(&bitmap[2..]);
    let mut frame = encode_vec(&FastPathHeader::new(
        EncryptionFlags::empty(),
        payload.len(),
    ))
    .unwrap();
    frame.extend_from_slice(&payload);
    frame
}

fn fastpath(update_code: UpdateCode, data: &[u8]) -> Vec<u8> {
    let payload = encode_vec(&FastPathUpdatePdu {
        fragmentation: Fragmentation::Single,
        update_code,
        compression_flags: None,
        compression_type: None,
        data,
    })
    .unwrap();
    let mut frame = encode_vec(&FastPathHeader::new(
        EncryptionFlags::empty(),
        payload.len(),
    ))
    .unwrap();
    frame.extend_from_slice(&payload);
    frame
}

#[test]
fn decoded_server_pixels_and_dirty_patch_match_the_display() {
    let dirty = InclusiveRectangle {
        left: 1,
        top: 0,
        right: 1,
        bottom: 1,
    };
    let pdu = bitmap_frame(dirty.clone());
    let mut inbound = Inbound::default();
    let mut active = ActiveStageBuilder {
        static_channels: Default::default(),
        user_channel_id: 1001,
        io_channel_id: 1003,
        message_channel_id: None,
        share_id: 1,
        compression_type: None,
        enable_server_pointer: false,
        pointer_software_rendering: false,
    }
    .build();
    let mut image = DecodedImage::new(PixelFormat::RgbA32, 2, 2);
    assert_eq!(
        inbound.check(Action::FastPath, &pdu[..pdu.len() - 1], &image),
        Err(Error::Protocol)
    );
    inbound.check(Action::FastPath, &pdu, &image).unwrap();
    let decoded = active
        .process(&mut image, Action::FastPath, &pdu)
        .unwrap()
        .into_iter()
        .find_map(|out| {
            if let ActiveStageOutput::GraphicsUpdate(rect) = out {
                Some(rect)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(decoded, dirty);
    let full = patch(SessionId(9), &image, &decoded, true).unwrap();
    assert_eq!(
        (full.session, full.width, full.height),
        (SessionId(9), 2, 2)
    );
    assert_eq!(
        full.rgba,
        [0, 0, 0, 255, 255, 0, 0, 255, 0, 0, 0, 255, 0, 0, 255, 255]
    );
    let delta = patch(SessionId(9), &image, &decoded, false).unwrap();
    assert_eq!(
        delta.rect,
        Rect {
            x: 1,
            y: 0,
            width: 1,
            height: 2
        }
    );
    assert_eq!(delta.rgba, [255, 0, 0, 255, 0, 0, 255, 255]);
    let outside = InclusiveRectangle {
        right: 2,
        ..dirty.clone()
    };
    assert_eq!(
        inbound.check(Action::FastPath, &bitmap_frame(outside.clone()), &image),
        Err(Error::Bounds)
    );
    assert!(matches!(
        patch(SessionId(9), &image, &outside, true),
        Err(Error::Bounds)
    ));
    assert!(matches!(
        patch(
            SessionId(9),
            &image,
            &InclusiveRectangle { left: 2, ..dirty },
            false
        ),
        Err(Error::Bounds)
    ));
    assert_eq!(frame_len(4096, 4096), Err(Error::Bounds));
    assert_eq!(frame_len(0, 1080), Err(Error::Bounds));
}

#[test]
fn clipboard_fragments_cannot_exceed_the_declared_and_bounded_message() {
    fn data(length: u32, flags: Flags, payload: &[u8]) -> Vec<u8> {
        let mut bytes = encode_vec(&ChannelPduHeader { length, flags }).unwrap();
        bytes.extend_from_slice(payload);
        bytes
    }
    let mut chunk = Chunk::default();
    assert_eq!(
        chunk.check(&data(1025, Flags::FLAG_FIRST, &[1]), 1024),
        Err(Error::Bounds)
    );
    chunk
        .check(&data(4, Flags::FLAG_FIRST, &[1, 2]), 1024)
        .unwrap();
    assert_eq!(
        chunk.check(&data(4, Flags::FLAG_LAST, &[3]), 1024),
        Err(Error::Bounds)
    );
    assert_eq!(
        chunk.check(&data(4, Flags::FLAG_LAST, &[3, 4, 5]), 1024),
        Err(Error::Bounds)
    );
    chunk
        .check(&data(4, Flags::FLAG_LAST, &[3, 4]), 1024)
        .unwrap();
}

#[test]
fn xrdp_compressed_bitmap_crops_source_stride_without_shear() {
    use ironrdp::graphics::rdp6::{BitmapStreamEncoder, RgbChannels};

    let (source_width, height) = (8usize, 3usize);
    let mut rgb = vec![0u8; source_width * height * 3];
    for y in 0..height {
        for x in 0..source_width {
            let index = (y * source_width + x) * 3;
            rgb[index] = (x as u8) * 10;
        }
    }
    let mut encoded = vec![0u8; source_width * height * 4 + 2];
    let written = BitmapStreamEncoder::new(source_width, height)
        .encode_bitmap::<RgbChannels>(&rgb, &mut encoded, true)
        .unwrap();
    let bitmap = BitmapUpdateData {
        rectangles: vec![BitmapData {
            rectangle: InclusiveRectangle { left: 0, top: 0, right: 6, bottom: 2 },
            width: 8,
            height: 3,
            bits_per_pixel: 32,
            compression_flags: Compression::BITMAP_COMPRESSION | Compression::NO_BITMAP_COMPRESSION_HDR,
            compressed_data_header: None,
            bitmap_data: &encoded[..written],
        }],
    };
    let data = encode_vec(&bitmap).unwrap();
    let update = encode_vec(&FastPathUpdatePdu {
        fragmentation: Fragmentation::Single,
        update_code: UpdateCode::Bitmap,
        compression_flags: None,
        compression_type: None,
        data: &data,
    }).unwrap();
    let mut pdu = encode_vec(&FastPathHeader::new(EncryptionFlags::empty(), update.len())).unwrap();
    pdu.extend_from_slice(&update);

    let (action, normalized) = super::normalize_bitmap(Action::FastPath, &pdu).unwrap().unwrap();
    assert_eq!(action, Action::FastPath);
    let mut input = ReadCursor::new(&normalized);
    let header: FastPathHeader = decode_cursor(&mut input).unwrap();
    assert_eq!(header.data_length, input.remaining().len());
    let update: FastPathUpdatePdu = decode_cursor(&mut input).unwrap();
    let out: BitmapUpdateData = decode_cursor(&mut ReadCursor::new(update.data)).unwrap();
    let rect = &out.rectangles[0];
    assert_eq!((rect.width, rect.height, rect.bits_per_pixel), (7, 3, 32));
    assert_eq!(rect.compression_flags, Compression::empty());
    assert_eq!(rect.bitmap_data.len(), 7 * 3 * 4);
    for y in 0..3 {
        for x in 0..7 {
            let index = (y * 7 + x) * 4;
            assert_eq!(
                &rect.bitmap_data[index..index + 4],
                &[0, 0, (x as u8) * 10, 0],
                "row {y} column {x} shifted"
            );
        }
    }
}
