use std::io::{Error, ErrorKind};

use rustedoutclient::vnc::{RfbError, RfbPhase};
use rustedoutclient_fuzz::{
    all_candidates, classify, execute_rfb_handshake, execute_rfb_hextile, execute_rfb_tight,
    execute_rfb_zrle, tight_geometry, write_candidates, SliceSink, CANONICAL_TARGETS,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn slice_sink_returns_all_finite_bytes_then_eof_and_discards_writes() {
    let mut sink = SliceSink::new(&[1, 2, 3]);
    let mut buffer = [0_u8; 2];

    assert_eq!(sink.read(&mut buffer).await.unwrap(), 2);
    assert_eq!(&buffer, &[1, 2]);
    assert_eq!(sink.read(&mut buffer).await.unwrap(), 1);
    assert_eq!(buffer[0], 3);
    assert_eq!(sink.read(&mut buffer).await.unwrap(), 0);
    sink.write_all(&[9, 8, 7]).await.unwrap();
    sink.flush().await.unwrap();
    sink.shutdown().await.unwrap();
}

#[test]
fn classify_distinguishes_eof_from_other_io_and_success() {
    let eof = RfbError::io(
        RfbPhase::Session,
        Error::new(ErrorKind::UnexpectedEof, "eof"),
    );
    let other = RfbError::io(
        RfbPhase::Session,
        Error::new(ErrorKind::ConnectionReset, "reset"),
    );
    assert_eq!(classify(Ok(())).as_str(), "Ok");
    assert_eq!(classify(Err(eof)).as_str(), "IoEof");
    assert_eq!(classify(Err(other)).as_str(), "IoOther");
}

#[test]
fn tight_geometry_byte_clamps_to_at_most_16x16() {
    assert_eq!(tight_geometry(0), (1, 1));
    assert_eq!(tight_geometry(1), (2, 2));
    assert_eq!(tight_geometry(2), (4, 4));
    assert_eq!(tight_geometry(3), (8, 8));
    assert_eq!(tight_geometry(4), (16, 16));
    assert_eq!(tight_geometry(5), (16, 16));
    assert_eq!(tight_geometry(255), (16, 16));
}

#[test]
fn canonical_target_list_is_exactly_five_in_order() {
    assert_eq!(
        CANONICAL_TARGETS,
        [
            "rfb_handshake",
            "rfb_session",
            "rfb_zrle",
            "rfb_tight",
            "rfb_hextile"
        ]
    );
}

fn candidate_bytes(target: &str, name: &str) -> Vec<u8> {
    all_candidates()
        .into_iter()
        .find(|candidate| candidate.target == target && candidate.name == name)
        .map(|candidate| candidate.bytes)
        .unwrap_or_else(|| panic!("missing candidate {target}/{name}"))
}

#[test]
fn handshake_valid_fixture_classifies_as_ok() {
    let execution = execute_rfb_handshake(&candidate_bytes(
        "rfb_handshake",
        "valid-3.8-vncauth-init.bin",
    ));
    assert_eq!(execution.category.as_str(), "Ok");
}

#[test]
fn handshake_malformed_banner_classifies_as_protocol_banner() {
    let execution =
        execute_rfb_handshake(&candidate_bytes("rfb_handshake", "banner-malformed.bin"));
    assert_eq!(execution.category.as_str(), "ProtocolBanner");
}

#[test]
fn zrle_solid_tile_classifies_as_ok() {
    let execution = execute_rfb_zrle(&candidate_bytes("rfb_zrle", "solid-tile.bin"));
    assert_eq!(execution.category.as_str(), "Ok");
}

#[test]
fn tight_fill_classifies_as_ok() {
    let execution = execute_rfb_tight(&candidate_bytes("rfb_tight", "fill.bin"));
    assert_eq!(execution.category.as_str(), "Ok");
}

#[test]
fn hextile_background_fill_classifies_as_ok() {
    let execution = execute_rfb_hextile(&candidate_bytes("rfb_hextile", "background-fill.bin"));
    assert_eq!(execution.category.as_str(), "Ok");
}

#[cfg(fuzzing)]
#[test]
fn session_valid_raw_rect_is_io_eof_with_framebuffer_event() {
    let execution = rustedoutclient_fuzz::execute_rfb_session(&candidate_bytes(
        "rfb_session",
        "valid-raw-rect.bin",
    ));
    assert_eq!(execution.category.as_str(), "IoEof");
    assert!(execution
        .transitions
        .iter()
        .any(|transition| transition == "framebuffer_event"));
}

#[cfg(fuzzing)]
#[test]
fn session_resize_fixtures_require_desktop_size_and_framebuffer_resized() {
    for name in ["desktop-size-resize.bin", "extended-desktop-size-valid.bin"] {
        let candidate = all_candidates()
            .into_iter()
            .find(|candidate| candidate.target == "rfb_session" && candidate.name == name)
            .unwrap();
        assert_eq!(
            candidate.transition,
            Some("desktop_size_event,framebuffer_resized"),
            "{name}"
        );
        let execution = rustedoutclient_fuzz::execute_rfb_session(&candidate.bytes);
        assert_eq!(execution.category.as_str(), "IoEof", "{name}");
        assert!(
            execution
                .transitions
                .iter()
                .any(|transition| transition == "desktop_size_event"),
            "{name} {:?}",
            execution.transitions
        );
        assert!(
            execution
                .transitions
                .iter()
                .any(|transition| transition == "framebuffer_resized"),
            "{name} {:?}",
            execution.transitions
        );
    }
}

#[cfg(fuzzing)]
#[test]
fn session_dispatcher_consumes_a_cursor_rectangle_as_typed_eof() {
    let mut bytes = vec![0, 0, 0, 1];
    bytes.extend_from_slice(&0_u16.to_be_bytes());
    bytes.extend_from_slice(&0_u16.to_be_bytes());
    bytes.extend_from_slice(&1_u16.to_be_bytes());
    bytes.extend_from_slice(&1_u16.to_be_bytes());
    bytes.extend_from_slice(&(-239_i32).to_be_bytes());
    bytes.extend_from_slice(&[1, 2, 3, 4, 0x80]);
    let execution = rustedoutclient_fuzz::execute_rfb_session(&bytes);
    assert_eq!(execution.category.as_str(), "IoEof");
    assert!(execution
        .transitions
        .iter()
        .all(|transition| transition != "framebuffer_event"));
}

#[test]
fn typed_builders_are_deterministic_across_two_private_directories() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first_manifest = write_candidates(first.path()).unwrap();
    let second_manifest = write_candidates(second.path()).unwrap();
    assert_eq!(first_manifest.seeds.len(), 62);
    assert_eq!(first_manifest.seeds, second_manifest.seeds);
    for seed in &first_manifest.seeds {
        let left = std::fs::read(first.path().join(&seed.file)).unwrap();
        let right = std::fs::read(second.path().join(&seed.file)).unwrap();
        assert_eq!(left, right, "{}", seed.file);
    }
}

#[test]
fn verify_rejects_parent_directory_seed_paths() {
    let root = tempfile::tempdir().unwrap();
    let manifest = rustedoutclient_fuzz::CorpusManifest {
        version: 1,
        targets: CANONICAL_TARGETS
            .iter()
            .map(|target| (*target).to_string())
            .collect(),
        seeds: vec![rustedoutclient_fuzz::SeedRecord {
            target: "rfb_handshake".to_string(),
            file: "../secret.bin".to_string(),
            sha256: "00".to_string(),
            length: 0,
            category: rustedoutclient_fuzz::Category::Ok,
            transition: None,
            fixture: None,
            reason: None,
            behavior: "escape".to_string(),
        }],
    };
    let failures = rustedoutclient_fuzz::verify_manifest(&manifest, root.path()).unwrap_err();
    assert!(failures.iter().any(|failure| failure.contains("unsafe")));
}

#[cfg(fuzzing)]
#[test]
fn all_typed_candidates_match_expected_categories() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_candidates(dir.path()).unwrap();
    rustedoutclient_fuzz::verify_manifest(&manifest, dir.path()).expect("candidate categories");
}
