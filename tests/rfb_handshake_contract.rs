use std::io;

use rustedoutclient::{
    connection::{bounded_vnc_channels, VncCommand, VncEvent, VNC_QUEUE_CAPACITY},
    vnc::{
        negotiate_security_type, negotiate_version, read_security_result, read_server_init,
        validate_framebuffer_layout, ProtocolLimits, RfbErrorKind, RfbPhase, RfbReader, RfbVersion,
    },
};
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

async fn negotiate_banner_fixture(
    banner: &[u8],
) -> Result<(RfbVersion, [u8; 12]), rustedoutclient::vnc::RfbError> {
    let (client, mut server) = duplex(64);
    server.write_all(banner).await.unwrap();
    server.shutdown().await.unwrap();

    let mut reader = RfbReader::new(client, ProtocolLimits::default());
    let version = negotiate_version(&mut reader).await?;
    let mut reply = [0_u8; 12];
    server.read_exact(&mut reply).await.unwrap();
    Ok((version, reply))
}

async fn modern_security_fixture(
    version: RfbVersion,
    offered: &[u8],
) -> Result<u8, rustedoutclient::vnc::RfbError> {
    let (client, mut server) = duplex(512);
    server.write_u8(offered.len() as u8).await.unwrap();
    server.write_all(offered).await.unwrap();
    server.shutdown().await.unwrap();

    let mut reader = RfbReader::new(client, ProtocolLimits::default());
    negotiate_security_type(&mut reader, version).await?;
    server
        .read_u8()
        .await
        .map_err(|source| rustedoutclient::vnc::RfbError::io(RfbPhase::SecurityTypes, source))
}

async fn legacy_security_fixture(security_type: u32) -> Result<(), rustedoutclient::vnc::RfbError> {
    let (client, mut server) = duplex(64);
    server.write_u32(security_type).await.unwrap();
    server.shutdown().await.unwrap();

    let mut reader = RfbReader::new(client, ProtocolLimits::default());
    negotiate_security_type(&mut reader, RfbVersion::V3_3).await
}

fn server_init_bytes(width: u16, height: u16, name: &[u8]) -> Vec<u8> {
    server_init_bytes_with_format(
        width,
        height,
        [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0],
        name,
    )
}

fn server_init_bytes_with_format(
    width: u16,
    height: u16,
    pixel_format: [u8; 16],
    name: &[u8],
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(24 + name.len());
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&pixel_format);
    bytes.extend_from_slice(&(name.len() as u32).to_be_bytes());
    bytes.extend_from_slice(name);
    bytes
}

async fn read_server_init_fixture(
    bytes: &[u8],
    limits: ProtocolLimits,
) -> Result<rustedoutclient::vnc::ServerInit, rustedoutclient::vnc::RfbError> {
    let (client, mut server) = duplex(bytes.len().max(64) + 1);
    server.write_all(bytes).await.unwrap();
    server.shutdown().await.unwrap();

    let mut reader = RfbReader::new(client, limits);
    read_server_init(&mut reader).await
}

#[test]
fn protocol_limits_are_the_exact_task_6_limits() {
    let limits = ProtocolLimits::default();
    assert_eq!(limits.max_dimension, 8_192);
    assert_eq!(limits.max_pixels, 33_554_432);
    assert_eq!(limits.max_framebuffer_bytes, 134_217_728);
    assert_eq!(limits.max_text_bytes, 65_536);
    assert_eq!(limits.max_rectangles, 4_096);
    assert_eq!(limits.max_encoded_rect_bytes, 67_108_864);
    assert_eq!(limits.max_clipboard_bytes, 1_048_576);
}

#[tokio::test]
async fn exact_supported_and_higher_numeric_banners_negotiate_without_downgrade() {
    for (banner, expected_version, expected_reply) in [
        (
            b"RFB 003.003\n".as_slice(),
            RfbVersion::V3_3,
            *b"RFB 003.003\n",
        ),
        (
            b"RFB 003.007\n".as_slice(),
            RfbVersion::V3_7,
            *b"RFB 003.007\n",
        ),
        (
            b"RFB 003.008\n".as_slice(),
            RfbVersion::V3_8,
            *b"RFB 003.008\n",
        ),
        (
            b"RFB 003.999\n".as_slice(),
            RfbVersion::V3_8,
            *b"RFB 003.008\n",
        ),
        (
            b"RFB 004.000\n".as_slice(),
            RfbVersion::V3_8,
            *b"RFB 003.008\n",
        ),
        (
            b"RFB 999.999\n".as_slice(),
            RfbVersion::V3_8,
            *b"RFB 003.008\n",
        ),
    ] {
        let (version, reply) = negotiate_banner_fixture(banner).await.unwrap();
        assert_eq!(version, expected_version, "banner {banner:?}");
        assert_eq!(reply, expected_reply, "banner {banner:?}");
    }
}

#[tokio::test]
async fn malformed_banner_is_rejected_instead_of_downgraded() {
    let unsupported = [
        b"RFB 003.000\n".as_slice(),
        b"RFB 003.001\n".as_slice(),
        b"RFB 003.002\n".as_slice(),
        b"RFB 003.004\n".as_slice(),
        b"RFB 003.005\n".as_slice(),
        b"RFB 003.006\n".as_slice(),
        b"RFB 002.999\n".as_slice(),
    ];
    for banner in unsupported {
        let err = negotiate_banner_fixture(banner).await.unwrap_err();
        assert_eq!(err.kind(), RfbErrorKind::ProtocolBanner, "{banner:?}");
        assert_eq!(err.phase(), RfbPhase::Banner);
    }

    let malformed: [&[u8]; 8] = [
        b"NOT RFB DATA",
        b"RFB 003.03x\n",
        b"RFB 003.008\r",
        b"RFB 003-008\n",
        b"RFB 003.008X",
        b"RFB 0003.008\n",
        b"RFB 1000.008\n",
        &[
            b'R', b'F', b'B', b' ', b'0', b'0', b'3', b'.', b'0', b'0', 0xff, b'\n',
        ],
    ];
    for banner in malformed {
        let err = negotiate_banner_fixture(banner).await.unwrap_err();
        assert_eq!(err.kind(), RfbErrorKind::ProtocolBanner, "{banner:?}");
        assert_eq!(err.phase(), RfbPhase::Banner);
    }
}

#[tokio::test]
async fn truncated_banner_preserves_unexpected_eof() {
    let err = negotiate_banner_fixture(b"RFB 003").await.unwrap_err();
    assert_eq!(err.phase(), RfbPhase::Banner);
    assert_eq!(err.io_kind(), Some(io::ErrorKind::UnexpectedEof));
}

#[tokio::test]
async fn only_vnc_auth_is_accepted_over_trusted_ssh() {
    for version in [RfbVersion::V3_7, RfbVersion::V3_8] {
        assert_eq!(modern_security_fixture(version, &[2]).await.unwrap(), 2);
        assert_eq!(
            modern_security_fixture(version, &[1, 5, 2, 6, 16, 19, 30, 129, 130])
                .await
                .unwrap(),
            2
        );

        for offered in [1, 5, 6, 16, 19, 30, 129, 130, 3, 200, 255] {
            let err = modern_security_fixture(version, &[offered])
                .await
                .unwrap_err();
            assert_eq!(err.kind(), RfbErrorKind::SecurityAllowlist);
            assert_eq!(err.phase(), RfbPhase::SecurityTypes);
        }
    }

    assert!(legacy_security_fixture(2).await.is_ok());
    for dictated in [1, 5, 6, 16, 19, 30, 129, 130, 3, 200, u32::MAX] {
        let err = legacy_security_fixture(dictated).await.unwrap_err();
        assert_eq!(err.kind(), RfbErrorKind::SecurityAllowlist);
        assert_eq!(err.phase(), RfbPhase::SecurityTypes);
    }
}

#[tokio::test]
async fn bounded_reason_accepts_65536_rejects_65537_and_preserves_truncation() {
    async fn failure_result(declared: u32, bytes: &[u8]) -> rustedoutclient::vnc::RfbError {
        let capacity = usize::try_from(declared).unwrap_or(0).min(65_537) + 16;
        let (client, mut server) = duplex(capacity);
        server.write_u32(1).await.unwrap();
        server.write_u32(declared).await.unwrap();
        server.write_all(bytes).await.unwrap();
        server.shutdown().await.unwrap();
        let mut reader = RfbReader::new(client, ProtocolLimits::default());
        read_security_result(&mut reader, RfbVersion::V3_8)
            .await
            .unwrap_err()
    }

    let accepted = vec![b'x'; 65_536];
    let err = failure_result(65_536, &accepted).await;
    assert_eq!(err.kind(), RfbErrorKind::SecurityFailure);

    let err = failure_result(65_537, &[]).await;
    assert_eq!(err.kind(), RfbErrorKind::Limit);
    assert_eq!(err.phase(), RfbPhase::SecurityResult);

    let err = failure_result(65_536, b"short").await;
    assert_eq!(err.io_kind(), Some(io::ErrorKind::UnexpectedEof));
    assert_eq!(err.phase(), RfbPhase::SecurityResult);
}

#[tokio::test]
async fn zero_security_type_reason_uses_the_same_bounded_reader() {
    async fn zero_type_failure(declared: u32, bytes: &[u8]) -> rustedoutclient::vnc::RfbError {
        let capacity = usize::try_from(declared).unwrap_or(0).min(65_537) + 16;
        let (client, mut server) = duplex(capacity);
        server.write_u8(0).await.unwrap();
        server.write_u32(declared).await.unwrap();
        server.write_all(bytes).await.unwrap();
        server.shutdown().await.unwrap();
        let mut reader = RfbReader::new(client, ProtocolLimits::default());
        negotiate_security_type(&mut reader, RfbVersion::V3_8)
            .await
            .unwrap_err()
    }

    let accepted = vec![b'x'; 65_536];
    let err = zero_type_failure(65_536, &accepted).await;
    assert_eq!(err.kind(), RfbErrorKind::SecurityFailure);
    assert_eq!(err.phase(), RfbPhase::SecurityTypes);

    let err = zero_type_failure(65_537, &[]).await;
    assert_eq!(err.kind(), RfbErrorKind::Limit);
    assert_eq!(err.phase(), RfbPhase::SecurityTypes);

    let err = zero_type_failure(65_536, b"short").await;
    assert_eq!(err.io_kind(), Some(io::ErrorKind::UnexpectedEof));
    assert_eq!(err.phase(), RfbPhase::SecurityTypes);
}

#[tokio::test]
async fn public_security_errors_preserve_kind_and_phase_without_remote_bytes() {
    let reason = b"ticket=SuperSecret challenge=response";
    let (client, mut server) = duplex(256);
    server.write_u32(1).await.unwrap();
    server.write_u32(reason.len() as u32).await.unwrap();
    server.write_all(reason).await.unwrap();
    server.shutdown().await.unwrap();
    let mut reader = RfbReader::new(client, ProtocolLimits::default());

    let err = read_security_result(&mut reader, RfbVersion::V3_8)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), RfbErrorKind::SecurityFailure);
    assert_eq!(err.phase(), RfbPhase::SecurityResult);
    let rendered = format!("{err:?} {err}");
    assert!(!rendered.contains("SuperSecret"));
    assert!(!rendered.contains("challenge=response"));
    assert!(rendered.len() <= 256);
}

#[tokio::test]
async fn bounded_reader_rejects_before_allocation_and_retains_io_kind() {
    let (client, mut server) = duplex(65_537);
    server.shutdown().await.unwrap();
    let mut reader = RfbReader::new(client, ProtocolLimits::default());
    let err = reader
        .read_bounded_bytes(65_537, 65_536, "desktop name", RfbPhase::ServerInit)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), RfbErrorKind::Limit);
    assert_eq!(err.phase(), RfbPhase::ServerInit);
    assert_eq!(err.io_kind(), None);

    let (client, mut server) = duplex(64);
    server.write_all(b"short").await.unwrap();
    server.shutdown().await.unwrap();
    let mut reader = RfbReader::new(client, ProtocolLimits::default());
    let err = reader
        .read_bounded_bytes(65_536, 65_536, "desktop name", RfbPhase::ServerInit)
        .await
        .unwrap_err();
    assert_eq!(err.io_kind(), Some(io::ErrorKind::UnexpectedEof));
}

#[test]
fn server_init_layout_rejects_zero_dimensions_and_checked_global_limits() {
    let limits = ProtocolLimits::default();
    assert!(validate_framebuffer_layout(8_192, 4_096, limits).is_ok());

    for (width, height) in [(0, 1), (1, 0), (8_193, 1), (1, 8_193), (8_192, 4_097)] {
        let err = validate_framebuffer_layout(width, height, limits).unwrap_err();
        assert!(matches!(
            err.kind(),
            RfbErrorKind::Limit | RfbErrorKind::ServerInit
        ));
        assert_eq!(err.phase(), RfbPhase::ServerInit);
    }

    let tiny_bytes = ProtocolLimits {
        max_framebuffer_bytes: 3,
        ..limits
    };
    let err = validate_framebuffer_layout(1, 1, tiny_bytes).unwrap_err();
    assert_eq!(err.kind(), RfbErrorKind::Limit);
}

#[tokio::test]
async fn protocol_limits_may_tighten_but_cannot_relax_exact_global_ceilings() {
    let relaxed_dimensions = ProtocolLimits {
        max_dimension: 8_193,
        ..ProtocolLimits::default()
    };
    let err = validate_framebuffer_layout(8_193, 1, relaxed_dimensions).unwrap_err();
    assert_eq!(err.kind(), RfbErrorKind::Limit);

    let relaxed_text = ProtocolLimits {
        max_text_bytes: 65_537,
        ..ProtocolLimits::default()
    };
    let oversized_name = vec![b'a'; 65_537];
    let err = read_server_init_fixture(&server_init_bytes(1, 1, &oversized_name), relaxed_text)
        .await
        .err()
        .expect("relaxed text limit must be rejected");
    assert_eq!(err.kind(), RfbErrorKind::Limit);
    assert_eq!(err.phase(), RfbPhase::ServerInit);
}

#[tokio::test]
async fn server_init_checks_dimensions_before_startup_allocation() {
    let bytes = server_init_bytes(8_193, 1, b"");
    let err = read_server_init_fixture(&bytes, ProtocolLimits::default())
        .await
        .err()
        .expect("oversized dimensions must fail");
    assert_eq!(err.kind(), RfbErrorKind::Limit);
    assert_eq!(err.phase(), RfbPhase::ServerInit);
}

#[tokio::test]
async fn server_init_rejects_structurally_invalid_true_colour_formats() {
    let invalid_formats = [
        // A channel maximum must be exactly 2^N - 1.
        [32, 24, 0, 1, 0, 254, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0],
        // Red and green masks overlap.
        [16, 16, 0, 1, 0, 31, 0, 63, 0, 31, 5, 5, 0, 0, 0, 0],
        // A five-bit red mask shifted by twelve extends beyond 16 bpp.
        [16, 16, 0, 1, 0, 31, 0, 63, 0, 31, 12, 5, 0, 0, 0, 0],
        // RGB565 has sixteen useful channel bits, not depth fifteen.
        [16, 15, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0],
        // Indexed colour is outside the native true-colour contract.
        [16, 16, 0, 0, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0],
        // Zero maxima, zero depth, unsupported bpp, and depth above bpp.
        [16, 16, 0, 1, 0, 0, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0],
        [16, 0, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0],
        [24, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0],
        [16, 17, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0],
    ];

    for pixel_format in invalid_formats {
        let err = read_server_init_fixture(
            &server_init_bytes_with_format(1, 1, pixel_format, b""),
            ProtocolLimits::default(),
        )
        .await
        .err()
        .expect("invalid pixel format must fail closed");
        assert_eq!(err.kind(), RfbErrorKind::ServerInit);
        assert_eq!(err.phase(), RfbPhase::ServerInit);
    }
}

#[tokio::test]
async fn bounded_desktop_name_accepts_65536_rejects_65537_and_sanitizes() {
    let accepted = vec![b'a'; 65_536];
    let init = read_server_init_fixture(
        &server_init_bytes(1, 1, &accepted),
        ProtocolLimits::default(),
    )
    .await
    .unwrap();
    assert_eq!(init.desktop_name.len(), 65_536);
    assert_eq!(init.framebuffer.len(), 4);

    let mut oversized = server_init_bytes(1, 1, b"");
    oversized[20..24].copy_from_slice(&65_537_u32.to_be_bytes());
    let err = read_server_init_fixture(&oversized, ProtocolLimits::default())
        .await
        .err()
        .expect("oversized desktop name must fail");
    assert_eq!(err.kind(), RfbErrorKind::Limit);

    let mut truncated = server_init_bytes(1, 1, b"short");
    truncated[20..24].copy_from_slice(&65_536_u32.to_be_bytes());
    let err = read_server_init_fixture(&truncated, ProtocolLimits::default())
        .await
        .err()
        .expect("truncated desktop name must fail");
    assert_eq!(err.io_kind(), Some(io::ErrorKind::UnexpectedEof));

    let init = read_server_init_fixture(
        &server_init_bytes(1, 1, b"Desk\0\n\xff"),
        ProtocolLimits::default(),
    )
    .await
    .unwrap();
    assert_eq!(init.desktop_name.len(), 7);
    assert!(init
        .desktop_name
        .bytes()
        .all(|byte| byte == b' ' || byte.is_ascii_graphic()));
}

#[test]
fn command_and_event_queues_are_exactly_capacity_256_and_nonblocking() {
    let (ui, session) = bounded_vnc_channels();
    assert_eq!(VNC_QUEUE_CAPACITY, 256);
    assert_eq!(ui.event_rx.capacity(), Some(256));
    assert_eq!(ui.command_tx.capacity(), Some(256));
    assert_eq!(session.event_tx.capacity(), Some(256));
    assert_eq!(session.command_rx.capacity(), Some(256));

    for _ in 0..256 {
        ui.command_tx.try_send(VncCommand::Disconnect).unwrap();
        session.event_tx.try_send(VncEvent::Disconnected).unwrap();
    }
    assert!(matches!(
        ui.command_tx.try_send(VncCommand::Disconnect),
        Err(crossbeam_channel::TrySendError::Full(_))
    ));
    assert!(matches!(
        session.event_tx.try_send(VncEvent::Disconnected),
        Err(crossbeam_channel::TrySendError::Full(_))
    ));
}

#[test]
fn raw_proxy_stream_cannot_call_public_vnc_api() {
    trybuild::TestCases::new().compile_fail("tests/ui/untrusted_stream.rs");
}
