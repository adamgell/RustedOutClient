use std::io::Write;

use flate2::{write::ZlibEncoder, Compression};
use image::{codecs::jpeg::JpegEncoder, ExtendedColorType};
use rustedoutclient::vnc::{
    encoding::{self, copyrect, hextile, raw, tight, zrle},
    messages::PixelFormat,
    CheckedRect, Framebuffer, ProtocolLimits, RfbError, RfbErrorKind, RfbPhase, RfbReader,
};

type CopyRectCase<'a> = (&'a [u8], u16, u16, u16, u16, RfbErrorKind);

fn limits() -> ProtocolLimits {
    ProtocolLimits {
        max_dimension: 128,
        max_pixels: 16_384,
        max_framebuffer_bytes: 65_536,
        max_encoded_rect_bytes: 65_536,
        ..ProtocolLimits::default()
    }
}

fn canonical_format() -> PixelFormat {
    PixelFormat {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_colour: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 16,
        green_shift: 8,
        blue_shift: 0,
    }
}

fn rgb565_in_32_format() -> PixelFormat {
    PixelFormat {
        bits_per_pixel: 32,
        depth: 16,
        big_endian: false,
        true_colour: true,
        red_max: 31,
        green_max: 63,
        blue_max: 31,
        red_shift: 11,
        green_shift: 5,
        blue_shift: 0,
    }
}

fn sparse_32_depth24_format() -> PixelFormat {
    PixelFormat {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_colour: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 24,
        green_shift: 8,
        blue_shift: 0,
    }
}

fn invalid_format() -> PixelFormat {
    PixelFormat {
        red_max: 0,
        ..canonical_format()
    }
}

fn framebuffer(width: u16, height: u16) -> Framebuffer {
    let mut framebuffer = Framebuffer::new(width, height, limits()).unwrap();
    let all = CheckedRect::new(0, 0, width, height, width, height).unwrap();
    framebuffer
        .write_rgba(
            all,
            &vec![0x5a; usize::from(width) * usize::from(height) * 4],
        )
        .unwrap();
    framebuffer
}

fn rgba(red: u8, green: u8, blue: u8) -> [u8; 4] {
    [red, green, blue, 255]
}

fn pixel32(red: u8, green: u8, blue: u8) -> [u8; 4] {
    [blue, green, red, 0]
}

fn cpixel(red: u8, green: u8, blue: u8) -> [u8; 3] {
    [blue, green, red]
}

fn tpixel(red: u8, green: u8, blue: u8) -> [u8; 3] {
    [red, green, blue]
}

fn full_snapshot(framebuffer: &Framebuffer) -> Vec<u8> {
    let (width, height) = framebuffer.dimensions();
    framebuffer
        .snapshot(CheckedRect::new(0, 0, width, height, width, height).unwrap())
        .unwrap()
}

fn assert_typed(error: &RfbError, phase: RfbPhase, kind: RfbErrorKind) {
    assert_eq!(error.phase(), phase);
    assert_eq!(error.kind(), kind);
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn zrle_wire(decoded: &[u8]) -> Vec<u8> {
    let compressed = zlib(decoded);
    let mut wire = Vec::new();
    wire.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    wire.extend_from_slice(&compressed);
    wire
}

fn compact_length(value: u32) -> Vec<u8> {
    assert!(value <= 4_194_303);
    if value <= 127 {
        vec![value as u8]
    } else if value <= 16_383 {
        vec![((value & 0x7f) as u8) | 0x80, (value >> 7) as u8]
    } else {
        vec![
            ((value & 0x7f) as u8) | 0x80,
            (((value >> 7) & 0x7f) as u8) | 0x80,
            (value >> 14) as u8,
        ]
    }
}

fn tight_compressed(control: u8, decoded: &[u8]) -> Vec<u8> {
    let compressed = zlib(decoded);
    let mut wire = vec![control];
    wire.extend_from_slice(&compact_length(compressed.len() as u32));
    wire.extend_from_slice(&compressed);
    wire
}

fn jpeg(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let mut pixels = vec![0; (width * height * 3) as usize];
    for pixel in pixels.chunks_exact_mut(3) {
        pixel.copy_from_slice(&rgb);
    }
    let mut encoded = Vec::new();
    JpegEncoder::new(&mut encoded)
        .encode(&pixels, width, height, ExtendedColorType::Rgb8)
        .unwrap();
    encoded
}

#[tokio::test]
async fn raw_minimal_pixel_uses_exact_payload_and_commits_once() {
    let mut framebuffer = framebuffer(2, 2);
    let wire = pixel32(0x11, 0x22, 0x33);
    let mut reader = RfbReader::new(&wire[..], limits());
    raw::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        1,
        1,
        1,
        1,
    )
    .await
    .unwrap();
    let target = CheckedRect::new(1, 1, 1, 1, 2, 2).unwrap();
    assert_eq!(
        framebuffer.snapshot(target).unwrap(),
        rgba(0x11, 0x22, 0x33)
    );
    assert!(reader.into_inner().is_empty());
}

#[tokio::test]
async fn raw_rejects_geometry_format_and_tightened_payload_limit_before_reading() {
    for (x, y, width, height, format, expected_kind) in [
        (2, 0, 1, 1, canonical_format(), RfbErrorKind::Protocol),
        (0, 0, 0, 1, canonical_format(), RfbErrorKind::Protocol),
        (0, 0, 1, 1, invalid_format(), RfbErrorKind::Decoder),
    ] {
        let mut framebuffer = framebuffer(2, 2);
        let before = full_snapshot(&framebuffer);
        let mut reader = RfbReader::new(&[][..], limits());
        let error = raw::decode(&mut reader, &mut framebuffer, &format, x, y, width, height)
            .await
            .unwrap_err();
        assert_typed(&error, RfbPhase::Encoding, expected_kind);
        assert_eq!(full_snapshot(&framebuffer), before);
    }

    let mut tightened = limits();
    tightened.max_encoded_rect_bytes = 3;
    let mut framebuffer = framebuffer(2, 2);
    let before = full_snapshot(&framebuffer);
    let mut reader = RfbReader::new(&[][..], tightened);
    let error = raw::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        1,
        1,
    )
    .await
    .unwrap_err();
    assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Limit);
    assert_eq!(full_snapshot(&framebuffer), before);

    let mut relaxed = limits();
    relaxed.max_encoded_rect_bytes = ProtocolLimits::default().max_encoded_rect_bytes + 1;
    let mut reader = RfbReader::new(&[][..], relaxed);
    let error = raw::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        1,
        1,
    )
    .await
    .unwrap_err();
    assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Limit);
    assert_eq!(full_snapshot(&framebuffer), before);
}

#[tokio::test]
async fn raw_truncation_never_mutates_the_framebuffer() {
    let mut framebuffer = framebuffer(2, 2);
    let before = full_snapshot(&framebuffer);
    let wire = [1, 2, 3];
    let mut reader = RfbReader::new(&wire[..], limits());
    let error = raw::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        1,
        1,
    )
    .await
    .unwrap_err();
    assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Io);
    assert_eq!(full_snapshot(&framebuffer), before);
}

#[tokio::test]
async fn copyrect_minimal_nonoverlap_and_overlap_are_exact() {
    let mut framebuffer = Framebuffer::new(4, 1, limits()).unwrap();
    let all = CheckedRect::new(0, 0, 4, 1, 4, 1).unwrap();
    framebuffer
        .write_rgba(
            all,
            &[rgba(1, 0, 0), rgba(2, 0, 0), rgba(3, 0, 0), rgba(4, 0, 0)].concat(),
        )
        .unwrap();

    let nonoverlap = [0, 0, 0, 0];
    let mut reader = RfbReader::new(&nonoverlap[..], limits());
    copyrect::decode(&mut reader, &mut framebuffer, 3, 0, 1, 1)
        .await
        .unwrap();
    assert_eq!(
        framebuffer.snapshot(all).unwrap(),
        [rgba(1, 0, 0), rgba(2, 0, 0), rgba(3, 0, 0), rgba(1, 0, 0),].concat()
    );

    let overlap = [0, 0, 0, 0];
    let mut reader = RfbReader::new(&overlap[..], limits());
    copyrect::decode(&mut reader, &mut framebuffer, 1, 0, 3, 1)
        .await
        .unwrap();
    assert_eq!(
        framebuffer.snapshot(all).unwrap(),
        [rgba(1, 0, 0), rgba(1, 0, 0), rgba(2, 0, 0), rgba(3, 0, 0),].concat()
    );
}

#[tokio::test]
async fn copyrect_rejects_invalid_source_and_destination_atomically() {
    let cases: [CopyRectCase<'_>; 2] = [
        (&[0, 3, 0, 3], 0, 0, 2, 2, RfbErrorKind::Protocol),
        (&[], 3, 3, 2, 2, RfbErrorKind::Protocol),
    ];
    for (wire, x, y, width, height, kind) in cases {
        let mut framebuffer = framebuffer(4, 4);
        let before = full_snapshot(&framebuffer);
        let mut reader = RfbReader::new(wire, limits());
        let error = copyrect::decode(&mut reader, &mut framebuffer, x, y, width, height)
            .await
            .unwrap_err();
        assert_typed(&error, RfbPhase::Encoding, kind);
        assert_eq!(full_snapshot(&framebuffer), before);
    }
}

#[tokio::test]
async fn copyrect_truncated_coordinates_are_typed_and_atomic() {
    let mut framebuffer = framebuffer(4, 4);
    let before = full_snapshot(&framebuffer);
    let mut reader = RfbReader::new(&[0, 1, 0][..], limits());
    let error = copyrect::decode(&mut reader, &mut framebuffer, 0, 0, 1, 1)
        .await
        .unwrap_err();
    assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Io);
    assert_eq!(full_snapshot(&framebuffer), before);
}

#[tokio::test]
async fn hextile_accepts_raw_with_irrelevant_bits_and_exact_nonraw_subrectangles() {
    let mut framebuffer = framebuffer(3, 1);
    let mut raw_wire = vec![0xff];
    raw_wire.extend_from_slice(&pixel32(1, 2, 3));
    let mut reader = RfbReader::new(&raw_wire[..], limits());
    hextile::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        1,
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        framebuffer
            .snapshot(CheckedRect::new(0, 0, 1, 1, 3, 1).unwrap())
            .unwrap(),
        rgba(1, 2, 3)
    );

    let mut nonraw = vec![0x0e];
    nonraw.extend_from_slice(&pixel32(10, 20, 30));
    nonraw.extend_from_slice(&pixel32(40, 50, 60));
    nonraw.extend_from_slice(&[1, 0, 0]);
    let mut reader = RfbReader::new(&nonraw[..], limits());
    hextile::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        1,
        0,
        2,
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        framebuffer
            .snapshot(CheckedRect::new(1, 0, 2, 1, 3, 1).unwrap())
            .unwrap(),
        [rgba(40, 50, 60), rgba(10, 20, 30)].concat()
    );
}

#[tokio::test]
async fn hextile_rejects_subrectangles_outside_the_current_edge_tile_without_clipping() {
    let mut wire = vec![0x0e];
    wire.extend_from_slice(&pixel32(10, 20, 30));
    wire.extend_from_slice(&pixel32(40, 50, 60));
    wire.extend_from_slice(&[1, 0, 0x10]);
    let mut framebuffer = framebuffer(1, 1);
    let before = full_snapshot(&framebuffer);
    let mut reader = RfbReader::new(&wire[..], limits());
    let error = hextile::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        1,
        1,
    )
    .await
    .unwrap_err();
    assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Decoder);
    assert_eq!(full_snapshot(&framebuffer), before);
}

#[tokio::test]
async fn hextile_truncated_count_and_subrectangle_payload_are_atomic() {
    for wire in [
        vec![0x0a, 0, 0, 0, 0],
        vec![0x0e, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0],
    ] {
        let mut framebuffer = framebuffer(1, 1);
        let before = full_snapshot(&framebuffer);
        let mut reader = RfbReader::new(&wire[..], limits());
        let error = hextile::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            1,
            1,
        )
        .await
        .unwrap_err();
        assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Io);
        assert_eq!(full_snapshot(&framebuffer), before);
    }
}

#[tokio::test]
async fn hextile_enforces_background_foreground_and_nonraw_bit_state_machine() {
    let mut cases = Vec::new();
    cases.push((1_u16, vec![0x00]));
    cases.push((1, {
        let mut wire = vec![0x0a];
        wire.extend_from_slice(&pixel32(1, 2, 3));
        wire.extend_from_slice(&[1, 0, 0]);
        wire
    }));
    cases.push((1, {
        let mut wire = vec![0x1e];
        wire.extend_from_slice(&pixel32(1, 2, 3));
        wire.extend_from_slice(&pixel32(4, 5, 6));
        wire.extend_from_slice(&[0]);
        wire
    }));
    cases.push((1, vec![0x20]));

    for (width, wire) in cases {
        let mut framebuffer = framebuffer(width, 1);
        let before = full_snapshot(&framebuffer);
        let mut reader = RfbReader::new(&wire[..], limits());
        let error = hextile::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            width,
            1,
        )
        .await
        .unwrap_err();
        assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Decoder);
        assert_eq!(full_snapshot(&framebuffer), before);
    }
}

#[tokio::test]
async fn hextile_raw_and_coloured_tiles_invalidate_carried_colour_state_atomically() {
    let mut after_raw = vec![0x06];
    after_raw.extend_from_slice(&pixel32(1, 1, 1));
    after_raw.extend_from_slice(&pixel32(2, 2, 2));
    after_raw.push(0x01);
    for _ in 0..16 {
        after_raw.extend_from_slice(&pixel32(3, 3, 3));
    }
    after_raw.push(0x00);

    let mut after_coloured = vec![0x06];
    after_coloured.extend_from_slice(&pixel32(1, 1, 1));
    after_coloured.extend_from_slice(&pixel32(2, 2, 2));
    after_coloured.push(0x18);
    after_coloured.extend_from_slice(&[1]);
    after_coloured.extend_from_slice(&pixel32(3, 3, 3));
    after_coloured.extend_from_slice(&[0, 0]);
    after_coloured.extend_from_slice(&[0x08, 1, 0, 0]);

    for (width, wire) in [(33, after_raw), (33, after_coloured)] {
        let mut framebuffer = framebuffer(width, 1);
        let before = full_snapshot(&framebuffer);
        let mut reader = RfbReader::new(&wire[..], limits());
        let error = hextile::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            width,
            1,
        )
        .await
        .unwrap_err();
        assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Decoder);
        assert_eq!(full_snapshot(&framebuffer), before);
    }
}

#[tokio::test]
async fn zrle_accepts_minimal_raw_solid_packed_plain_rle_and_palette_rle_tiles() {
    let fixtures = [
        [vec![0], cpixel(1, 2, 3).to_vec()].concat(),
        [vec![1], cpixel(1, 2, 3).to_vec()].concat(),
        [
            vec![2],
            cpixel(1, 2, 3).to_vec(),
            cpixel(9, 8, 7).to_vec(),
            vec![0],
        ]
        .concat(),
        [vec![128], cpixel(1, 2, 3).to_vec(), vec![0]].concat(),
        [
            vec![130],
            cpixel(1, 2, 3).to_vec(),
            cpixel(9, 8, 7).to_vec(),
            vec![0],
        ]
        .concat(),
    ];

    for decoded in fixtures {
        let wire = zrle_wire(&decoded);
        let mut reader = RfbReader::new(&wire[..], limits());
        let mut framebuffer = framebuffer(1, 1);
        zrle::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            1,
            1,
            &mut zrle::ZrleState::new(),
        )
        .await
        .unwrap();
        assert_eq!(full_snapshot(&framebuffer), rgba(1, 2, 3));
    }
}

#[tokio::test]
async fn zrle_rejects_u32_over_limit_before_payload_read_and_accepts_exact_tightened_limit() {
    let mut framebuffer = framebuffer(1, 1);
    let before = full_snapshot(&framebuffer);
    let declared = 67_108_865_u32.to_be_bytes();
    let mut reader = RfbReader::new(&declared[..], ProtocolLimits::default());
    let error = zrle::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        1,
        1,
        &mut zrle::ZrleState::new(),
    )
    .await
    .unwrap_err();
    assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Limit);
    assert_eq!(full_snapshot(&framebuffer), before);

    let wire = zrle_wire(&[vec![1], cpixel(4, 5, 6).to_vec()].concat());
    let compressed_length = u32::from_be_bytes(wire[..4].try_into().unwrap());
    let mut exact_limits = limits();
    exact_limits.max_encoded_rect_bytes = compressed_length;
    let mut reader = RfbReader::new(&wire[..], exact_limits);
    zrle::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        1,
        1,
        &mut zrle::ZrleState::new(),
    )
    .await
    .unwrap();
    assert_eq!(full_snapshot(&framebuffer), rgba(4, 5, 6));
}

#[tokio::test]
async fn zrle_rejects_palette_indexes_unused_subtypes_and_truncated_fields_atomically() {
    let invalid = [
        [
            vec![3],
            cpixel(1, 1, 1).to_vec(),
            cpixel(2, 2, 2).to_vec(),
            cpixel(3, 3, 3).to_vec(),
            vec![0xc0],
        ]
        .concat(),
        vec![17],
        vec![129],
        vec![0, 1, 2],
        vec![2, 1, 2, 3],
        [vec![128], cpixel(1, 2, 3).to_vec()].concat(),
    ];
    for decoded in invalid {
        let wire = zrle_wire(&decoded);
        let mut reader = RfbReader::new(&wire[..], limits());
        let mut framebuffer = framebuffer(1, 1);
        let before = full_snapshot(&framebuffer);
        let error = zrle::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            1,
            1,
            &mut zrle::ZrleState::new(),
        )
        .await
        .unwrap_err();
        assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Decoder);
        assert_eq!(full_snapshot(&framebuffer), before);
    }
}

#[tokio::test]
async fn zrle_rejects_runs_beyond_remaining_pixels_before_expansion() {
    for decoded in [
        [vec![128], cpixel(1, 2, 3).to_vec(), vec![1]].concat(),
        [
            vec![130],
            cpixel(1, 2, 3).to_vec(),
            cpixel(4, 5, 6).to_vec(),
            vec![0x80, 1],
        ]
        .concat(),
    ] {
        let wire = zrle_wire(&decoded);
        let mut reader = RfbReader::new(&wire[..], limits());
        let mut framebuffer = framebuffer(1, 1);
        let before = full_snapshot(&framebuffer);
        let error = zrle::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            1,
            1,
            &mut zrle::ZrleState::new(),
        )
        .await
        .unwrap_err();
        assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Decoder);
        assert_eq!(full_snapshot(&framebuffer), before);
    }
}

#[tokio::test]
async fn zrle_geometry_bound_accepts_its_tiny_palette_rle_edge_and_rejects_trailing_or_missing_tiles(
) {
    let mut edge = vec![255];
    for value in 0..127_u8 {
        edge.extend_from_slice(&cpixel(value, value, value));
    }
    edge.push(0);
    assert_eq!(
        edge.len(),
        zrle::max_decompressed_bytes(1, 1, &canonical_format()).unwrap()
    );

    let wire = zrle_wire(&edge);
    let mut reader = RfbReader::new(&wire[..], limits());
    let mut edge_framebuffer = framebuffer(1, 1);
    zrle::decode(
        &mut reader,
        &mut edge_framebuffer,
        &canonical_format(),
        0,
        0,
        1,
        1,
        &mut zrle::ZrleState::new(),
    )
    .await
    .unwrap();

    let trailing_below_ceiling = [vec![1], cpixel(7, 8, 9).to_vec(), vec![0]].concat();
    assert!(
        trailing_below_ceiling.len()
            < zrle::max_decompressed_bytes(1, 1, &canonical_format()).unwrap()
    );

    for (decoded, expected_kind) in [
        (vec![], RfbErrorKind::Decoder),
        (trailing_below_ceiling, RfbErrorKind::Decoder),
        ([edge, vec![0]].concat(), RfbErrorKind::Limit),
    ] {
        let wire = zrle_wire(&decoded);
        let mut reader = RfbReader::new(&wire[..], limits());
        let mut framebuffer = framebuffer(1, 1);
        let before = full_snapshot(&framebuffer);
        let error = zrle::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            1,
            1,
            &mut zrle::ZrleState::new(),
        )
        .await
        .unwrap_err();
        assert_typed(&error, RfbPhase::Encoding, expected_kind);
        assert_eq!(full_snapshot(&framebuffer), before);
    }
}

#[tokio::test]
async fn zrle_malformed_or_no_progress_zlib_stream_is_finite_and_atomic() {
    for compressed in [vec![], vec![0x78], vec![0x78, 0x9c, 0xff]] {
        let mut wire = Vec::new();
        wire.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
        wire.extend_from_slice(&compressed);
        let mut reader = RfbReader::new(&wire[..], limits());
        let mut framebuffer = framebuffer(1, 1);
        let before = full_snapshot(&framebuffer);
        let error = zrle::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            1,
            1,
            &mut zrle::ZrleState::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.phase(), RfbPhase::Encoding);
        assert!(matches!(
            error.kind(),
            RfbErrorKind::Decoder | RfbErrorKind::Io
        ));
        assert_eq!(full_snapshot(&framebuffer), before);
    }
}

#[tokio::test]
async fn zrle_cpixel_width_requires_channels_to_fit_three_contiguous_bytes() {
    let decoded = [vec![1], vec![0x00, 0x00, 0x00, 0xff]].concat();
    let wire = zrle_wire(&decoded);
    let mut reader = RfbReader::new(&wire[..], limits());
    let mut framebuffer = framebuffer(1, 1);
    zrle::decode(
        &mut reader,
        &mut framebuffer,
        &sparse_32_depth24_format(),
        0,
        0,
        1,
        1,
        &mut zrle::ZrleState::new(),
    )
    .await
    .unwrap();
    assert_eq!(full_snapshot(&framebuffer), rgba(255, 0, 0));
}

#[tokio::test]
async fn tight_control_high_nibble_selects_fill_and_low_nibble_resets_streams() {
    let mut wire = vec![0x80];
    wire.extend_from_slice(&tpixel(1, 2, 3));
    let mut reader = RfbReader::new(&wire[..], limits());
    let mut framebuffer = framebuffer(2, 2);
    tight::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        2,
        2,
        &mut tight::TightState::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        full_snapshot(&framebuffer),
        [rgba(1, 2, 3), rgba(1, 2, 3), rgba(1, 2, 3), rgba(1, 2, 3)].concat()
    );
}

#[tokio::test]
async fn tight_reset_bits_apply_to_the_low_nibble_with_stateful_continuation() {
    let first = vec![1_u8; 18];
    let second = vec![2_u8; 18];
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&first).unwrap();
    encoder.flush().unwrap();
    let split = encoder.get_ref().len();
    let first_segment = encoder.get_ref().clone();
    encoder.write_all(&second).unwrap();
    encoder.flush().unwrap();
    let second_segment = encoder.get_ref()[split..].to_vec();

    let mut state = tight::TightState::new();
    let mut framebuffer = framebuffer(3, 2);
    let mut first_wire = vec![0x12];
    first_wire.extend_from_slice(&compact_length(first_segment.len() as u32));
    first_wire.extend_from_slice(&first_segment);
    let mut reader = RfbReader::new(&first_wire[..], limits());
    tight::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        3,
        2,
        &mut state,
    )
    .await
    .unwrap();

    let mut second_wire = vec![0x10];
    second_wire.extend_from_slice(&compact_length(second_segment.len() as u32));
    second_wire.extend_from_slice(&second_segment);
    let mut reader = RfbReader::new(&second_wire[..], limits());
    tight::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        3,
        2,
        &mut state,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn tight_accepts_minimal_basic_copy_gradient_palette_fill_and_jpeg_forms() {
    let fixtures = [
        [vec![0x00], tpixel(1, 2, 3).to_vec()].concat(),
        [vec![0x40, 0x02], tpixel(1, 2, 3).to_vec()].concat(),
        [
            vec![0x40, 0x01, 1],
            tpixel(1, 2, 3).to_vec(),
            tpixel(9, 8, 7).to_vec(),
            vec![0],
        ]
        .concat(),
        [vec![0x80], tpixel(1, 2, 3).to_vec()].concat(),
    ];
    for wire in fixtures {
        let mut framebuffer = framebuffer(1, 1);
        let mut reader = RfbReader::new(&wire[..], limits());
        tight::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            1,
            1,
            &mut tight::TightState::new(),
        )
        .await
        .unwrap();
        assert_eq!(full_snapshot(&framebuffer), rgba(1, 2, 3));
    }

    let gradient = [
        tpixel(10, 20, 30),
        tpixel(10, 20, 30),
        tpixel(20, 40, 60),
        tpixel(0, 0, 0),
    ]
    .concat();
    let compressed = zlib(&gradient);
    let mut wire = vec![0x40, 0x02];
    wire.extend_from_slice(&compact_length(compressed.len() as u32));
    wire.extend_from_slice(&compressed);
    let mut gradient_framebuffer = framebuffer(2, 2);
    let mut reader = RfbReader::new(&wire[..], limits());
    tight::decode(
        &mut reader,
        &mut gradient_framebuffer,
        &canonical_format(),
        0,
        0,
        2,
        2,
        &mut tight::TightState::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        full_snapshot(&gradient_framebuffer),
        [
            rgba(10, 20, 30),
            rgba(20, 40, 60),
            rgba(30, 60, 90),
            rgba(40, 80, 120),
        ]
        .concat()
    );

    let encoded = jpeg(1, 1, [30, 40, 50]);
    let mut wire = vec![0x90];
    wire.extend_from_slice(&compact_length(encoded.len() as u32));
    wire.extend_from_slice(&encoded);
    let mut framebuffer = framebuffer(1, 1);
    let mut reader = RfbReader::new(&wire[..], limits());
    tight::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        1,
        1,
        &mut tight::TightState::new(),
    )
    .await
    .unwrap();
    assert_ne!(full_snapshot(&framebuffer), vec![0x5a; 4]);
}

#[tokio::test]
async fn tight_width_above_2048_rejects_before_payload_read_and_mutation() {
    let wide_limits = ProtocolLimits {
        max_framebuffer_bytes: 8_196 * 4,
        ..ProtocolLimits::default()
    };
    let mut framebuffer = Framebuffer::new(2_049, 1, wide_limits).unwrap();
    let before = full_snapshot(&framebuffer);
    let mut reader = RfbReader::new(&[][..], wide_limits);
    let error = tight::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        2_049,
        1,
        &mut tight::TightState::new(),
    )
    .await
    .unwrap_err();
    assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Limit);
    assert_eq!(full_snapshot(&framebuffer), before);
}

#[tokio::test]
async fn tight_compact_length_has_exact_canonical_boundaries_and_maximum() {
    for value in [0, 127, 128, 16_383, 16_384, 4_194_303] {
        let wire = compact_length(value);
        let mut reader = RfbReader::new(&wire[..], limits());
        assert_eq!(
            tight::read_compact_length(&mut reader).await.unwrap(),
            value
        );
        assert!(reader.into_inner().is_empty());
    }

    for wire in [
        vec![0x80, 0x00],
        vec![0x80, 0x80, 0x00],
        vec![0x80],
        vec![0x80, 0x80],
    ] {
        let mut reader = RfbReader::new(&wire[..], limits());
        let error = tight::read_compact_length(&mut reader).await.unwrap_err();
        assert_eq!(error.phase(), RfbPhase::Encoding);
        assert!(matches!(
            error.kind(),
            RfbErrorKind::Decoder | RfbErrorKind::Io
        ));
    }
}

#[tokio::test]
async fn tight_representable_declared_length_obeys_tightened_limit_before_read() {
    let mut tightened = limits();
    tightened.max_encoded_rect_bytes = 127;
    let mut wire = vec![0x90];
    wire.extend_from_slice(&compact_length(128));
    let mut reader = RfbReader::new(&wire[..], tightened);
    let mut framebuffer = framebuffer(1, 1);
    let before = full_snapshot(&framebuffer);
    let error = tight::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        1,
        1,
        &mut tight::TightState::new(),
    )
    .await
    .unwrap_err();
    assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Limit);
    assert_eq!(full_snapshot(&framebuffer), before);
}

#[tokio::test]
async fn tight_basic_requires_zlib_and_exact_short_long_or_progressing_output() {
    let invalid = [
        tight_compressed(0x00, &[0; 17]),
        tight_compressed(0x00, &[0; 19]),
        vec![0xa0],
        vec![0xe0],
        {
            let mut wire = vec![0x00];
            wire.extend_from_slice(&compact_length(1));
            wire.push(0x78);
            wire
        },
        {
            let mut wire = vec![0x00];
            wire.extend_from_slice(&compact_length(18));
            wire.extend_from_slice(&[0; 18]);
            wire
        },
    ];
    for wire in invalid {
        let mut framebuffer = framebuffer(3, 2);
        let before = full_snapshot(&framebuffer);
        let mut reader = RfbReader::new(&wire[..], limits());
        let error = tight::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            3,
            2,
            &mut tight::TightState::new(),
        )
        .await
        .unwrap_err();
        assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Decoder);
        assert_eq!(full_snapshot(&framebuffer), before);
    }
}

#[tokio::test]
async fn tight_palette_requires_two_to_256_colours_and_strict_indexes() {
    let invalid = [
        [vec![0x40, 0x01, 0], tpixel(1, 2, 3).to_vec(), vec![0]].concat(),
        [
            vec![0x40, 0x01, 2],
            tpixel(1, 2, 3).to_vec(),
            tpixel(4, 5, 6).to_vec(),
            tpixel(7, 8, 9).to_vec(),
            vec![3],
        ]
        .concat(),
    ];
    for wire in invalid {
        let mut framebuffer = framebuffer(1, 1);
        let before = full_snapshot(&framebuffer);
        let mut reader = RfbReader::new(&wire[..], limits());
        let error = tight::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            1,
            1,
            &mut tight::TightState::new(),
        )
        .await
        .unwrap_err();
        assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Decoder);
        assert_eq!(full_snapshot(&framebuffer), before);
    }

    let mut wire = vec![0x40, 0x01, 255];
    for value in 0..=255_u8 {
        wire.extend_from_slice(&tpixel(value, value, value));
    }
    wire.push(255);
    let mut framebuffer = framebuffer(1, 1);
    let mut reader = RfbReader::new(&wire[..], limits());
    tight::decode(
        &mut reader,
        &mut framebuffer,
        &canonical_format(),
        0,
        0,
        1,
        1,
        &mut tight::TightState::new(),
    )
    .await
    .unwrap();
    assert_eq!(full_snapshot(&framebuffer), rgba(255, 255, 255));
}

#[tokio::test]
async fn tight_jpeg_checks_length_dimensions_and_decode_before_framebuffer_commit() {
    let wrong_dimensions = jpeg(2, 1, [10, 20, 30]);
    let mut wrong_wire = vec![0x90];
    wrong_wire.extend_from_slice(&compact_length(wrong_dimensions.len() as u32));
    wrong_wire.extend_from_slice(&wrong_dimensions);

    for wire in [wrong_wire, vec![0x90, 3, 1, 2, 3]] {
        let mut framebuffer = framebuffer(1, 1);
        let before = full_snapshot(&framebuffer);
        let mut reader = RfbReader::new(&wire[..], limits());
        let error = tight::decode(
            &mut reader,
            &mut framebuffer,
            &canonical_format(),
            0,
            0,
            1,
            1,
            &mut tight::TightState::new(),
        )
        .await
        .unwrap_err();
        assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Decoder);
        assert_eq!(full_snapshot(&framebuffer), before);
    }
}

#[tokio::test]
async fn tight_tpixel_width_follows_protocol_conditions_for_32_bpp_depth_16() {
    let wire = [0x80, 0x00, 0xf8, 0x00, 0x00];
    let mut reader = RfbReader::new(&wire[..], limits());
    let mut framebuffer = framebuffer(1, 1);
    tight::decode(
        &mut reader,
        &mut framebuffer,
        &rgb565_in_32_format(),
        0,
        0,
        1,
        1,
        &mut tight::TightState::new(),
    )
    .await
    .unwrap();
    assert_eq!(full_snapshot(&framebuffer), rgba(255, 0, 0));
    assert!(reader.into_inner().is_empty());
}

#[test]
fn desktop_size_is_transactional_and_uses_the_same_framebuffer_limits() {
    let mut framebuffer = framebuffer(2, 2);
    let before = full_snapshot(&framebuffer);
    let error = encoding::decode_desktop_size(&mut framebuffer, 129, 1).unwrap_err();
    assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Limit);
    assert_eq!(framebuffer.dimensions(), (2, 2));
    assert_eq!(full_snapshot(&framebuffer), before);

    encoding::decode_desktop_size(&mut framebuffer, 3, 1).unwrap();
    assert_eq!(framebuffer.dimensions(), (3, 1));
    assert_eq!(framebuffer.pixels(), &[0; 12]);
}

#[tokio::test]
async fn cursor_hotspot_is_not_a_framebuffer_destination_and_exact_payload_is_bounded() {
    let wire = [1, 2, 3, 4, 0x80];
    let mut reader = RfbReader::new(&wire[..], limits());
    encoding::decode_cursor(&mut reader, &canonical_format(), u16::MAX, u16::MAX, 1, 1)
        .await
        .unwrap();
    assert!(reader.into_inner().is_empty());
}

#[tokio::test]
async fn cursor_truncation_and_tightened_over_limit_shapes_fail_closed() {
    let sentinel = framebuffer(2, 2);
    let before = full_snapshot(&sentinel);

    let mut reader = RfbReader::new(&[1, 2, 3, 4][..], limits());
    let error = encoding::decode_cursor(&mut reader, &canonical_format(), 0, 0, 1, 1)
        .await
        .unwrap_err();
    assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Io);
    assert_eq!(full_snapshot(&sentinel), before);

    let mut tightened = limits();
    tightened.max_encoded_rect_bytes = 4;
    let mut reader = RfbReader::new(&[][..], tightened);
    let error = encoding::decode_cursor(&mut reader, &canonical_format(), 0, 0, 1, 1)
        .await
        .unwrap_err();
    assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Limit);
    assert_eq!(full_snapshot(&sentinel), before);

    let mut pixel_limited = limits();
    pixel_limited.max_pixels = 0;
    let mut reader = RfbReader::new(&[][..], pixel_limited);
    let error = encoding::decode_cursor(&mut reader, &canonical_format(), 0, 0, 1, 1)
        .await
        .unwrap_err();
    assert_typed(&error, RfbPhase::Encoding, RfbErrorKind::Limit);
    assert_eq!(full_snapshot(&sentinel), before);
}
