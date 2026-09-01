use std::{
    io::{Error, ErrorKind},
    process::Command,
};

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
    assert_eq!(failures, vec!["FAIL seed unsafe".to_string()]);
}

fn canonical_manifest(
    seed: rustedoutclient_fuzz::SeedRecord,
) -> rustedoutclient_fuzz::CorpusManifest {
    rustedoutclient_fuzz::CorpusManifest {
        version: 1,
        targets: CANONICAL_TARGETS
            .iter()
            .map(|target| (*target).to_string())
            .collect(),
        seeds: vec![seed],
    }
}

fn write_hextile_invalid_subtype(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("rfb_hextile")).unwrap();
    std::fs::write(root.join("rfb_hextile/invalid-subtype-bits.bin"), [0x20]).unwrap();
}

fn hextile_seed(file: &str) -> rustedoutclient_fuzz::SeedRecord {
    rustedoutclient_fuzz::SeedRecord {
        target: "rfb_hextile".to_string(),
        file: file.to_string(),
        sha256: "36a9e7f1c95b82ffb99743e0c5c4ce95d83c9a430aac59f84ef3cbfab6145068".to_string(),
        length: 1,
        category: rustedoutclient_fuzz::Category::Decoder,
        transition: None,
        fixture: None,
        reason: None,
        behavior: "unknown subtype bit 0x20".to_string(),
    }
}

#[test]
fn verify_length_mismatch_prints_token_without_lengths() {
    let root = tempfile::tempdir().unwrap();
    write_hextile_invalid_subtype(root.path());
    let manifest = canonical_manifest(rustedoutclient_fuzz::SeedRecord {
        target: "rfb_hextile".to_string(),
        file: "rfb_hextile/invalid-subtype-bits.bin".to_string(),
        sha256: "00".to_string(),
        length: 8,
        category: rustedoutclient_fuzz::Category::Decoder,
        transition: None,
        fixture: None,
        reason: None,
        behavior: "unknown subtype bit 0x20".to_string(),
    });
    let failures = rustedoutclient_fuzz::verify_manifest(&manifest, root.path()).unwrap_err();
    assert_eq!(failures, vec!["FAIL seed length-mismatch".to_string()]);
}

#[test]
fn verify_hash_mismatch_prints_token_without_digests() {
    let root = tempfile::tempdir().unwrap();
    write_hextile_invalid_subtype(root.path());
    let manifest = canonical_manifest(rustedoutclient_fuzz::SeedRecord {
        target: "rfb_hextile".to_string(),
        file: "rfb_hextile/invalid-subtype-bits.bin".to_string(),
        sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        length: 1,
        category: rustedoutclient_fuzz::Category::Decoder,
        transition: None,
        fixture: None,
        reason: None,
        behavior: "unknown subtype bit 0x20".to_string(),
    });
    let failures = rustedoutclient_fuzz::verify_manifest(&manifest, root.path()).unwrap_err();
    assert_eq!(failures, vec!["FAIL seed hash-mismatch".to_string()]);
}

#[test]
fn verify_transition_mismatch_prints_token_without_debug_collection() {
    let root = tempfile::tempdir().unwrap();
    write_hextile_invalid_subtype(root.path());
    let manifest = canonical_manifest(rustedoutclient_fuzz::SeedRecord {
        target: "rfb_hextile".to_string(),
        file: "rfb_hextile/invalid-subtype-bits.bin".to_string(),
        sha256: "36a9e7f1c95b82ffb99743e0c5c4ce95d83c9a430aac59f84ef3cbfab6145068".to_string(),
        length: 1,
        category: rustedoutclient_fuzz::Category::Decoder,
        transition: Some("framebuffer_event".to_string()),
        fixture: None,
        reason: None,
        behavior: "unknown subtype bit 0x20".to_string(),
    });
    let failures = rustedoutclient_fuzz::verify_manifest(&manifest, root.path()).unwrap_err();
    assert_eq!(failures, vec!["FAIL seed transition-mismatch".to_string()]);
}

#[test]
fn verify_missing_seed_prints_token_without_path() {
    let root = tempfile::tempdir().unwrap();
    let manifest = canonical_manifest(rustedoutclient_fuzz::SeedRecord {
        target: "rfb_hextile".to_string(),
        file: "rfb_hextile/absent.bin".to_string(),
        sha256: "00".to_string(),
        length: 1,
        category: rustedoutclient_fuzz::Category::Decoder,
        transition: None,
        fixture: None,
        reason: None,
        behavior: "missing".to_string(),
    });
    let failures = rustedoutclient_fuzz::verify_manifest(&manifest, root.path()).unwrap_err();
    assert_eq!(failures, vec!["FAIL seed missing".to_string()]);
}

#[test]
fn verify_unknown_target_does_not_echo_arbitrary_manifest_input() {
    let root = tempfile::tempdir().unwrap();
    write_hextile_invalid_subtype(root.path());
    let manifest = canonical_manifest(rustedoutclient_fuzz::SeedRecord {
        target: "/tmp/secret-ticket\nINJECT".to_string(),
        file: "rfb_hextile/invalid-subtype-bits.bin".to_string(),
        sha256: "36a9e7f1c95b82ffb99743e0c5c4ce95d83c9a430aac59f84ef3cbfab6145068".to_string(),
        length: 1,
        category: rustedoutclient_fuzz::Category::Decoder,
        transition: None,
        fixture: None,
        reason: None,
        behavior: "unknown subtype bit 0x20".to_string(),
    });
    let failures = rustedoutclient_fuzz::verify_manifest(&manifest, root.path()).unwrap_err();
    assert_eq!(failures, vec!["FAIL seed unknown-target".to_string()]);
    assert!(!failures[0].contains("secret-ticket"));
    assert!(!failures[0].contains("/tmp"));
    assert!(!failures[0].contains('\n'));
}

#[test]
fn verify_rejects_a_canonical_target_that_disagrees_with_the_file_directory() {
    let root = tempfile::tempdir().unwrap();
    write_hextile_invalid_subtype(root.path());
    let manifest = canonical_manifest(rustedoutclient_fuzz::SeedRecord {
        target: "rfb_handshake".to_string(),
        file: "rfb_hextile/invalid-subtype-bits.bin".to_string(),
        sha256: "36a9e7f1c95b82ffb99743e0c5c4ce95d83c9a430aac59f84ef3cbfab6145068".to_string(),
        length: 1,
        category: rustedoutclient_fuzz::Category::IoEof,
        transition: None,
        fixture: None,
        reason: None,
        behavior: "target mismatch".to_string(),
    });
    let failures = rustedoutclient_fuzz::verify_manifest(&manifest, root.path()).unwrap_err();
    assert_eq!(failures, vec!["FAIL seed target-mismatch".to_string()]);
}

#[test]
fn verify_invalid_basename_is_replaced_and_cannot_forge_a_second_line() {
    let root = tempfile::tempdir().unwrap();
    let manifest = canonical_manifest(hextile_seed("rfb_hextile/evil\nINJECT.bin"));
    let failures = rustedoutclient_fuzz::verify_manifest(&manifest, root.path()).unwrap_err();
    assert_eq!(failures, vec!["FAIL seed filename".to_string()]);
    assert!(!failures[0].contains("evil"));
    assert!(!failures[0].contains("INJECT"));
    assert!(!failures[0].contains('\n'));
}

#[test]
fn verify_rejects_duplicate_file_strings_before_a_second_dispatch() {
    let root = tempfile::tempdir().unwrap();
    write_hextile_invalid_subtype(root.path());
    let seed = hextile_seed("rfb_hextile/invalid-subtype-bits.bin");
    let manifest = rustedoutclient_fuzz::CorpusManifest {
        version: 1,
        targets: CANONICAL_TARGETS
            .iter()
            .map(|target| (*target).to_string())
            .collect(),
        seeds: vec![seed.clone(), seed],
    };
    let failures = rustedoutclient_fuzz::verify_manifest(&manifest, root.path()).unwrap_err();
    assert_eq!(failures, vec!["FAIL seed duplicate-file".to_string()]);
}

#[cfg(unix)]
#[test]
fn verify_rejects_distinct_names_that_resolve_to_one_file() {
    let root = tempfile::tempdir().unwrap();
    write_hextile_invalid_subtype(root.path());
    std::os::unix::fs::symlink(
        "invalid-subtype-bits.bin",
        root.path().join("rfb_hextile/alias.bin"),
    )
    .unwrap();
    let manifest = rustedoutclient_fuzz::CorpusManifest {
        version: 1,
        targets: CANONICAL_TARGETS
            .iter()
            .map(|target| (*target).to_string())
            .collect(),
        seeds: vec![
            hextile_seed("rfb_hextile/invalid-subtype-bits.bin"),
            hextile_seed("rfb_hextile/alias.bin"),
        ],
    };
    let failures = rustedoutclient_fuzz::verify_manifest(&manifest, root.path()).unwrap_err();
    assert_eq!(failures, vec!["FAIL seed duplicate-file".to_string()]);
}

#[test]
fn load_manifest_missing_file_omits_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.json");
    let error = rustedoutclient_fuzz::load_manifest(&path).unwrap_err();
    assert_eq!(error, "could not read manifest");
    assert!(!error.contains(&path.display().to_string()));
}

#[test]
fn load_manifest_invalid_json_omits_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.json");
    std::fs::write(&path, "not-json").unwrap();
    let error = rustedoutclient_fuzz::load_manifest(&path).unwrap_err();
    assert_eq!(error, "could not parse manifest");
    assert!(!error.contains(&path.display().to_string()));
}

#[test]
fn verify_seeds_unknown_argument_omits_input() {
    let output = Command::new(env!("CARGO_BIN_EXE_verify_seeds"))
        .arg("--/tmp/secret-ticket")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr,
        "usage: verify_seeds [--manifest PATH] [--root DIR]\nunknown argument\n"
    );
    assert!(!stderr.contains("secret-ticket"));
    assert!(!stderr.contains("/tmp"));
}

#[test]
fn verify_seeds_length_mismatch_stderr_is_token_only() {
    let root = tempfile::tempdir().unwrap();
    write_hextile_invalid_subtype(root.path());
    let manifest_path = root.path().join("candidate-manifest.json");
    std::fs::write(
        &manifest_path,
        r#"{
  "version": 1,
  "targets": ["rfb_handshake", "rfb_session", "rfb_zrle", "rfb_tight", "rfb_hextile"],
  "seeds": [{
    "target": "rfb_hextile",
    "file": "rfb_hextile/invalid-subtype-bits.bin",
    "sha256": "00",
    "length": 8,
    "category": "Decoder",
    "behavior": "unknown subtype bit 0x20"
  }]
}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_verify_seeds"))
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--root")
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr, "FAIL seed length-mismatch\n");
}

#[test]
fn verify_seeds_failed_manifest_emits_no_seed_names_or_partial_success() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("rfb_hextile")).unwrap();
    std::fs::write(root.path().join("rfb_hextile/customer-ticket.bin"), [0x20]).unwrap();
    let manifest_path = root.path().join("candidate-manifest.json");
    std::fs::write(
        &manifest_path,
        r#"{
  "version": 1,
  "targets": ["rfb_handshake", "rfb_session", "rfb_zrle", "rfb_tight", "rfb_hextile"],
  "seeds": [{
    "target": "rfb_hextile",
    "file": "rfb_hextile/customer-ticket.bin",
    "sha256": "36a9e7f1c95b82ffb99743e0c5c4ce95d83c9a430aac59f84ef3cbfab6145068",
    "length": 1,
    "category": "Decoder",
    "behavior": "valid first record"
  }, {
    "target": "rfb_hextile",
    "file": "rfb_hextile/secret-session.bin",
    "sha256": "00",
    "length": 1,
    "category": "Decoder",
    "behavior": "failing second record"
  }]
}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_verify_seeds"))
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--root")
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stdout, "");
    assert_eq!(stderr, "FAIL seed missing\n");
    let combined = format!("{stdout}{stderr}");
    assert!(!combined.contains("customer-ticket"));
    assert!(!combined.contains("secret-session"));
}

#[test]
fn verify_seeds_unknown_target_stderr_does_not_echo_manifest_input() {
    let root = tempfile::tempdir().unwrap();
    write_hextile_invalid_subtype(root.path());
    let manifest_path = root.path().join("candidate-manifest.json");
    std::fs::write(
        &manifest_path,
        r#"{
  "version": 1,
  "targets": ["rfb_handshake", "rfb_session", "rfb_zrle", "rfb_tight", "rfb_hextile"],
  "seeds": [{
    "target": "/tmp/secret-ticket\nINJECT",
    "file": "rfb_hextile/invalid-subtype-bits.bin",
    "sha256": "36a9e7f1c95b82ffb99743e0c5c4ce95d83c9a430aac59f84ef3cbfab6145068",
    "length": 1,
    "category": "Decoder",
    "behavior": "unknown target"
  }]
}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_verify_seeds"))
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--root")
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr, "FAIL seed unknown-target\n");
    assert!(!stderr.contains("secret-ticket"));
    assert!(!stderr.contains("/tmp"));
    assert_eq!(stderr.lines().count(), 1);
}

#[test]
fn verify_seeds_invalid_basename_stderr_is_one_fixed_line() {
    let root = tempfile::tempdir().unwrap();
    let manifest_path = root.path().join("candidate-manifest.json");
    std::fs::write(
        &manifest_path,
        r#"{
  "version": 1,
  "targets": ["rfb_handshake", "rfb_session", "rfb_zrle", "rfb_tight", "rfb_hextile"],
  "seeds": [{
    "target": "rfb_hextile",
    "file": "rfb_hextile/evil\nINJECT.bin",
    "sha256": "00",
    "length": 1,
    "category": "Decoder",
    "behavior": "invalid basename"
  }]
}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_verify_seeds"))
        .arg("--manifest")
        .arg(&manifest_path)
        .arg("--root")
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr, "FAIL seed filename\n");
    assert!(!stderr.contains("evil"));
    assert!(!stderr.contains("INJECT"));
    assert_eq!(stderr.lines().count(), 1);
}

#[cfg(fuzzing)]
#[test]
fn all_typed_candidates_match_expected_categories() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_candidates(dir.path()).unwrap();
    rustedoutclient_fuzz::verify_manifest(&manifest, dir.path()).expect("candidate categories");
}
