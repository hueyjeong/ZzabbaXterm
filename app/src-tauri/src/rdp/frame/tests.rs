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

fn test_image() -> DecodedImage {
    DecodedImage::new(PixelFormat::RgbA32, 1024, 768)
}

fn fastpath_frame(updates: &[(Fragmentation, UpdateCode, &[u8])]) -> Vec<u8> {
    let mut payload = Vec::new();
    for (fragmentation, code, data) in updates {
        payload.extend_from_slice(&encode_vec(&FastPathUpdatePdu {
            fragmentation: *fragmentation,
            update_code: *code,
            compression_flags: None,
            compression_type: None,
            data,
        }).unwrap());
    }
    let mut pdu = encode_vec(&FastPathHeader::new(EncryptionFlags::empty(), payload.len())).unwrap();
    pdu.extend_from_slice(&payload);
    pdu
}

fn encoded_bitmap(rect: InclusiveRectangle, width: u16, height: u16, bpp: u16, flags: Compression, data: &[u8]) -> Vec<u8> {
    encode_vec(&BitmapUpdateData {
        rectangles: vec![BitmapData {
            rectangle: rect, width, height, bits_per_pixel: bpp,
            compression_flags: flags, compressed_data_header: None, bitmap_data: data,
        }],
    }).unwrap()
}

fn rdp6(width: usize, height: usize) -> (Vec<u8>, Vec<u8>) {
    use ironrdp::graphics::rdp6::{BitmapStreamEncoder, RgbChannels};
    let mut rgb = vec![0u8; width * height * 3];
    for y in 0..height {
        for x in 0..width {
            rgb[(y * width + x) * 3] = x as u8;
        }
    }
    let mut encoded = vec![0u8; width * height * 4 + 2];
    let written = BitmapStreamEncoder::new(width, height)
        .encode_bitmap::<RgbChannels>(&rgb, &mut encoded, true).unwrap();
    encoded.truncate(written);
    (encoded, rgb)
}

fn output_bitmaps(normalized: Normalized) -> Vec<BitmapUpdateData<'static>> {
    let Normalized::Replace(pdus) = normalized else { panic!("expected replacement") };
    let mut out = Vec::new();
    for (action, pdu) in pdus {
        assert_eq!(action, Action::FastPath);
        let mut input = ReadCursor::new(&pdu);
        let header: FastPathHeader = decode_cursor(&mut input).unwrap();
        assert!(header.data_length == input.remaining().len());
        while !input.is_empty() {
            let update: FastPathUpdatePdu<'_> = decode_cursor(&mut input).unwrap();
            if update.update_code == UpdateCode::Bitmap {
                let bitmap: BitmapUpdateData<'_> = decode_cursor(&mut ReadCursor::new(update.data)).unwrap();
                out.push(BitmapUpdateData {
                    rectangles: bitmap.rectangles.iter().map(|rect| BitmapData {
                        rectangle: rect.rectangle.clone(), width: rect.width, height: rect.height,
                        bits_per_pixel: rect.bits_per_pixel, compression_flags: rect.compression_flags,
                        compressed_data_header: None, bitmap_data: Box::leak(rect.bitmap_data.to_vec().into_boxed_slice()),
                    }).collect(),
                });
            }
        }
    }
    out
}

#[test]
fn xrdp_compressed_bitmap_crops_source_stride_without_shear() {
    let (encoded, _) = rdp6(8, 3);
    let data = encoded_bitmap(InclusiveRectangle { left: 0, top: 0, right: 6, bottom: 2 }, 8, 3, 32,
        Compression::BITMAP_COMPRESSION | Compression::NO_BITMAP_COMPRESSION_HDR, &encoded);
    let pdu = fastpath_frame(&[(Fragmentation::Single, UpdateCode::Bitmap, &data)]);
    let normalized = Normalizer::default().normalize(Action::FastPath, &pdu, &test_image()).unwrap();
    let bitmaps = output_bitmaps(normalized);
    let rect = &bitmaps[0].rectangles[0];
    assert_eq!((rect.width, rect.height, rect.bits_per_pixel), (7, 3, 32));
    assert_eq!(rect.compression_flags, Compression::empty());
    for y in 0..3 {
        for x in 0..7 {
            let index = (y * 7 + x) * 4;
            assert_eq!(&rect.bitmap_data[index..index + 4], &[0, 0, x as u8, 0], "row {y} column {x} shifted");
        }
    }
}

#[test]
fn oversized_declared_bitmap_is_rejected_before_decoding() {
    let data = encoded_bitmap(InclusiveRectangle { left: 0, top: 0, right: 10, bottom: 9 }, 65535, 10, 32,
        Compression::BITMAP_COMPRESSION | Compression::NO_BITMAP_COMPRESSION_HDR, &[]);
    let pdu = fastpath_frame(&[(Fragmentation::Single, UpdateCode::Bitmap, &data)]);
    assert!(matches!(Normalizer::default().normalize(Action::FastPath, &pdu, &test_image()), Err(Error::Bounds)));
}

#[test]
fn large_cropped_bitmap_is_split_into_bounded_pdus_with_correct_pixels() {
    let (encoded, _) = rdp6(1024, 30);
    let data = encoded_bitmap(InclusiveRectangle { left: 0, top: 0, right: 1020, bottom: 29 }, 1024, 30, 32,
        Compression::BITMAP_COMPRESSION | Compression::NO_BITMAP_COMPRESSION_HDR, &encoded);
    let pdu = fastpath_frame(&[(Fragmentation::Single, UpdateCode::Bitmap, &data)]);
    let normalized = Normalizer::default().normalize(Action::FastPath, &pdu, &test_image()).unwrap();
    let Normalized::Replace(pdus) = &normalized else { panic!("expected replacement") };
    assert!(pdus.len() > 1);
    assert!(pdus.iter().all(|(_, pdu)| pdu.len() <= 32_767));
    let bitmaps = output_bitmaps(normalized);
    let mut bands = bitmaps.iter().map(|bitmap| bitmap.rectangles[0].rectangle.clone()).collect::<Vec<_>>();
    bands.sort_by_key(|rect| rect.top);
    assert_eq!(bands.first().map(|rect| rect.top), Some(0));
    assert_eq!(bands.last().map(|rect| rect.bottom), Some(29));
    for pair in bands.windows(2) {
        assert_eq!(pair[1].top, pair[0].bottom + 1);
    }
    let rows: u32 = bitmaps.iter().map(|bitmap| u32::from(bitmap.rectangles[0].height)).sum();
    assert_eq!(rows, 30);
    for bitmap in &bitmaps {
        let rect = &bitmap.rectangles[0];
        assert_eq!(rect.width, 1021);
        let first = rect.bitmap_data[..4].to_vec();
        assert_eq!(first, vec![0, 0, 0, 0]);
        let last = rect.bitmap_data[(1021 * 4) - 4..(1021 * 4)].to_vec();
        assert_eq!(last, vec![0, 0, 252, 0]);
    }
}

#[test]
fn fragmented_compressed_bitmap_is_reassembled_and_normalized() {
    let (encoded, _) = rdp6(8, 3);
    let data = encoded_bitmap(InclusiveRectangle { left: 0, top: 0, right: 6, bottom: 2 }, 8, 3, 32,
        Compression::BITMAP_COMPRESSION | Compression::NO_BITMAP_COMPRESSION_HDR, &encoded);
    let split = data.len() / 2;
    let first = fastpath_frame(&[(Fragmentation::First, UpdateCode::Bitmap, &data[..split])]);
    let next = fastpath_frame(&[(Fragmentation::Next, UpdateCode::Bitmap, &data[split..])]);
    let mut normalizer = Normalizer::default();
    assert!(matches!(normalizer.normalize(Action::FastPath, &first, &test_image()).unwrap(), Normalized::Drop));
    assert!(matches!(normalizer.normalize(Action::FastPath, &next, &test_image()).unwrap(), Normalized::Drop));
    let last = fastpath_frame(&[(Fragmentation::Last, UpdateCode::Bitmap, &[])]);
    let normalized = normalizer.normalize(Action::FastPath, &last, &test_image()).unwrap();
    let bitmaps = output_bitmaps(normalized);
    assert_eq!(bitmaps[0].rectangles[0].width, 7);
    assert_eq!(&bitmaps[0].rectangles[0].bitmap_data[..4], &[0, 0, 0, 0]);
}

#[test]
fn pointer_update_sharing_a_pdu_with_a_bitmap_is_kept() {
    let (encoded, _) = rdp6(8, 3);
    let data = encoded_bitmap(InclusiveRectangle { left: 0, top: 0, right: 6, bottom: 2 }, 8, 3, 32,
        Compression::BITMAP_COMPRESSION | Compression::NO_BITMAP_COMPRESSION_HDR, &encoded);
    let pointer = [0x0a, 0, 0, 0, 0, 0, 0, 0];
    let pdu = fastpath_frame(&[
        (Fragmentation::Single, UpdateCode::PositionPointer, &pointer),
        (Fragmentation::Single, UpdateCode::Bitmap, &data),
    ]);
    let normalized = Normalizer::default().normalize(Action::FastPath, &pdu, &test_image()).unwrap();
    let Normalized::Replace(pdus) = normalized else { panic!("expected replacement") };
    let mut codes = Vec::new();
    for (_, pdu) in pdus {
        let mut input = ReadCursor::new(&pdu);
        let _: FastPathHeader = decode_cursor(&mut input).unwrap();
        while !input.is_empty() {
            let update: FastPathUpdatePdu<'_> = decode_cursor(&mut input).unwrap();
            codes.push(update.update_code);
        }
    }
    assert!(codes.contains(&UpdateCode::PositionPointer));
    assert!(codes.contains(&UpdateCode::Bitmap));
}

#[test]
fn fragmented_non_bitmap_update_keeps_its_fragmentation() {
    let (encoded, _) = rdp6(8, 3);
    let data = encoded_bitmap(InclusiveRectangle { left: 0, top: 0, right: 6, bottom: 2 }, 8, 3, 32,
        Compression::BITMAP_COMPRESSION | Compression::NO_BITMAP_COMPRESSION_HDR, &encoded);
    let pdu = fastpath_frame(&[
        (Fragmentation::First, UpdateCode::SurfaceCommands, &[0u8; 4]),
        (Fragmentation::Single, UpdateCode::Bitmap, &data),
    ]);
    let Normalized::Replace(pdus) = Normalizer::default().normalize(Action::FastPath, &pdu, &test_image()).unwrap()
        else { panic!("expected replacement") };
    let mut seen = Vec::new();
    for (_, pdu) in pdus {
        let mut input = ReadCursor::new(&pdu);
        let _: FastPathHeader = decode_cursor(&mut input).unwrap();
        while !input.is_empty() {
            let update: FastPathUpdatePdu<'_> = decode_cursor(&mut input).unwrap();
            seen.push((update.update_code, update.fragmentation));
        }
    }
    assert!(seen.contains(&(UpdateCode::SurfaceCommands, Fragmentation::First)));
}

#[test]
fn fragment_buffer_cap_is_bounds() {
    let mut buffer = vec![0u8; 5];
    assert!(matches!(super::extend_fragment(&mut buffer, &[0u8; 6], 10), Err(Error::Bounds)));
}

#[test]
fn invalid_bits_per_pixel_is_bounds_without_panic() {
    for bits in [0u16, 7] {
        let data = encoded_bitmap(InclusiveRectangle { left: 0, top: 0, right: 1, bottom: 1 }, 2, 2, bits, Compression::empty(), &[0; 32]);
        let pdu = fastpath_frame(&[(Fragmentation::Single, UpdateCode::Bitmap, &data)]);
        assert!(matches!(Normalizer::default().normalize(Action::FastPath, &pdu, &test_image()), Err(Error::Bounds)));
    }
}

#[test]
fn compressed_low_depth_bitmap_without_crop_passes_through() {
    let data = encoded_bitmap(InclusiveRectangle { left: 0, top: 0, right: 3, bottom: 1 }, 4, 2, 16,
        Compression::BITMAP_COMPRESSION | Compression::NO_BITMAP_COMPRESSION_HDR, &[0; 8]);
    let pdu = fastpath_frame(&[(Fragmentation::Single, UpdateCode::Bitmap, &data)]);
    assert!(matches!(Normalizer::default().normalize(Action::FastPath, &pdu, &test_image()).unwrap(), Normalized::Pass));
}

#[test]
fn a_single_bitmap_drops_an_older_fragment() {
    let (encoded, _) = rdp6(8, 3);
    let data = encoded_bitmap(InclusiveRectangle { left: 0, top: 0, right: 6, bottom: 2 }, 8, 3, 32,
        Compression::BITMAP_COMPRESSION | Compression::NO_BITMAP_COMPRESSION_HDR, &encoded);
    let first = fastpath_frame(&[(Fragmentation::First, UpdateCode::Bitmap, &data)]);
    let mut normalizer = Normalizer::default();
    assert!(matches!(normalizer.normalize(Action::FastPath, &first, &test_image()).unwrap(), Normalized::Drop));
    let single = fastpath_frame(&[(Fragmentation::Single, UpdateCode::Bitmap, &data)]);
    assert!(matches!(normalizer.normalize(Action::FastPath, &single, &test_image()).unwrap(), Normalized::Replace(_)));
    let orphan = fastpath_frame(&[(Fragmentation::Last, UpdateCode::Bitmap, &[])]);
    assert!(matches!(normalizer.normalize(Action::FastPath, &orphan, &test_image()), Err(Error::Protocol)));
}

#[test]
fn a_pointer_next_to_a_bitmap_fragment_is_not_dropped() {
    let pdu = fastpath_frame(&[
        (Fragmentation::First, UpdateCode::Bitmap, &[0u8; 4]),
        (Fragmentation::Single, UpdateCode::PositionPointer, &[1, 0, 2, 0]),
    ]);
    let Normalized::Replace(pdus) = Normalizer::default().normalize(Action::FastPath, &pdu, &test_image()).unwrap()
        else { panic!("expected replacement") };
    let mut input = ReadCursor::new(&pdus[0].1);
    let _: FastPathHeader = decode_cursor(&mut input).unwrap();
    let update: FastPathUpdatePdu<'_> = decode_cursor(&mut input).unwrap();
    assert_eq!((update.update_code, update.data), (UpdateCode::PositionPointer, &[1u8, 0, 2, 0][..]));
}
