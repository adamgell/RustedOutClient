use rustedoutclient::vnc::{
    validate_framebuffer_layout, CheckedRect, Framebuffer, ProtocolLimits, RfbErrorKind, RfbPhase,
};

fn small_limits() -> ProtocolLimits {
    ProtocolLimits {
        max_dimension: 64,
        max_pixels: 4_096,
        max_framebuffer_bytes: 16_384,
        ..ProtocolLimits::default()
    }
}

fn sentinel_framebuffer(width: u16, height: u16) -> Framebuffer {
    let mut framebuffer = Framebuffer::new(width, height, small_limits()).unwrap();
    let all = CheckedRect::new(0, 0, width, height, width, height).unwrap();
    let bytes = vec![0x5a; usize::from(width) * usize::from(height) * 4];
    framebuffer.write_rgba(all, &bytes).unwrap();
    framebuffer
}

fn rgba(values: &[u8]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| [*value, 0, 0, 255])
        .collect()
}

#[test]
fn framebuffer_layout_enforces_exact_global_dimension_pixel_and_byte_limits_without_allocating() {
    let limits = ProtocolLimits::default();
    let exact = validate_framebuffer_layout(8_192, 4_096, limits).unwrap();
    assert_eq!(exact.pixels, 33_554_432);
    assert_eq!(exact.rgba_bytes, 134_217_728);

    for (width, height) in [(8_193, 1), (1, 8_193), (8_192, 8_192)] {
        let error = validate_framebuffer_layout(width, height, limits).unwrap_err();
        assert_eq!(error.kind(), RfbErrorKind::Limit);
    }

    let mut byte_limited = limits;
    byte_limited.max_framebuffer_bytes = 15;
    let error = validate_framebuffer_layout(2, 2, byte_limited).unwrap_err();
    assert_eq!(error.kind(), RfbErrorKind::Limit);
}

#[test]
fn checked_rect_rejects_zero_overflow_and_each_one_past_edge() {
    for result in [
        CheckedRect::new(0, 0, 0, 1, 8, 8),
        CheckedRect::new(0, 0, 1, 0, 8, 8),
        CheckedRect::new(u16::MAX, 0, 1, 1, u16::MAX, 8),
        CheckedRect::new(0, u16::MAX, 1, 1, 8, u16::MAX),
        CheckedRect::new(7, 0, 2, 1, 8, 8),
        CheckedRect::new(0, 7, 1, 2, 8, 8),
    ] {
        let error = result.unwrap_err();
        assert_eq!(error.phase(), RfbPhase::Framebuffer);
        assert!(matches!(
            error.kind(),
            RfbErrorKind::Protocol | RfbErrorKind::Limit
        ));
    }

    let exact = CheckedRect::new(6, 6, 2, 2, 8, 8).unwrap();
    assert_eq!(
        (exact.x(), exact.y(), exact.width(), exact.height()),
        (6, 6, 2, 2)
    );
}

#[test]
fn framebuffer_exact_write_succeeds_and_short_or_long_input_is_atomic() {
    let mut framebuffer = sentinel_framebuffer(3, 2);
    let target = CheckedRect::new(1, 0, 2, 2, 3, 2).unwrap();
    let exact = rgba(&[1, 2, 3, 4]);
    framebuffer.write_rgba(target, &exact).unwrap();
    assert_eq!(framebuffer.snapshot(target).unwrap(), exact);

    for invalid in [&exact[..exact.len() - 1], &[0_u8; 17][..]] {
        let before = framebuffer.pixels().to_vec();
        let error = framebuffer.write_rgba(target, invalid).unwrap_err();
        assert_eq!(error.phase(), RfbPhase::Framebuffer);
        assert_eq!(error.kind(), RfbErrorKind::Protocol);
        assert_eq!(framebuffer.pixels(), before);
    }
}

#[test]
fn framebuffer_resize_is_transactional_on_validation_failure_and_exact_on_success() {
    let mut framebuffer = sentinel_framebuffer(3, 2);
    let before = framebuffer.pixels().to_vec();
    let before_dimensions = framebuffer.dimensions();

    let error = framebuffer.resize(65, 1).unwrap_err();
    assert_eq!(error.kind(), RfbErrorKind::Limit);
    assert_eq!(framebuffer.dimensions(), before_dimensions);
    assert_eq!(framebuffer.pixels(), before);

    framebuffer.resize(2, 3).unwrap();
    assert_eq!(framebuffer.dimensions(), (2, 3));
    assert_eq!(framebuffer.pixels(), &[0; 24]);
}

#[test]
fn framebuffer_copyrect_supports_nonoverlap_and_memmove_overlap() {
    let mut framebuffer = Framebuffer::new(4, 2, small_limits()).unwrap();
    let top = CheckedRect::new(0, 0, 4, 1, 4, 2).unwrap();
    let bottom = CheckedRect::new(0, 1, 4, 1, 4, 2).unwrap();
    framebuffer.write_rgba(top, &rgba(&[1, 2, 3, 4])).unwrap();
    framebuffer.copy_rect(bottom, 0, 0).unwrap();
    assert_eq!(framebuffer.snapshot(bottom).unwrap(), rgba(&[1, 2, 3, 4]));

    let overlap = CheckedRect::new(1, 0, 3, 1, 4, 2).unwrap();
    framebuffer.copy_rect(overlap, 0, 0).unwrap();
    assert_eq!(framebuffer.snapshot(top).unwrap(), rgba(&[1, 1, 2, 3]));
}

#[test]
fn framebuffer_copyrect_validates_source_and_destination_before_mutation() {
    let mut framebuffer = sentinel_framebuffer(4, 4);

    let valid_destination = CheckedRect::new(0, 0, 2, 2, 4, 4).unwrap();
    let before = framebuffer.pixels().to_vec();
    let error = framebuffer.copy_rect(valid_destination, 3, 3).unwrap_err();
    assert_eq!(error.phase(), RfbPhase::Framebuffer);
    assert_eq!(error.kind(), RfbErrorKind::Protocol);
    assert_eq!(framebuffer.pixels(), before);

    let destination_for_other_frame = CheckedRect::new(0, 0, 2, 2, 5, 5).unwrap();
    let error = framebuffer
        .copy_rect(destination_for_other_frame, 0, 0)
        .unwrap_err();
    assert_eq!(error.kind(), RfbErrorKind::Protocol);
    assert_eq!(framebuffer.pixels(), before);
}

#[test]
fn framebuffer_snapshot_rejects_out_of_bounds_instead_of_clamping() {
    let framebuffer = sentinel_framebuffer(4, 4);
    let exact = CheckedRect::new(3, 3, 1, 1, 4, 4).unwrap();
    assert_eq!(framebuffer.snapshot(exact).unwrap(), vec![0x5a; 4]);

    let from_other_frame = CheckedRect::new(3, 3, 2, 2, 5, 5).unwrap();
    let error = framebuffer.snapshot(from_other_frame).unwrap_err();
    assert_eq!(error.phase(), RfbPhase::Framebuffer);
    assert_eq!(error.kind(), RfbErrorKind::Protocol);
}
