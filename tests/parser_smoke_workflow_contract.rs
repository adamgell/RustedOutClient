use std::{env, fs, path::Path};

const CHECKOUT_PIN: &str = "actions/checkout@11d5960a326750d5838078e36cf38b85af677262";

fn workflow() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    fs::read_to_string(root.join(".github/workflows/parser-smoke.yml")).unwrap()
}

#[test]
fn parser_smoke_workflow_exists_with_required_triggers_and_runner() {
    let workflow = workflow();
    assert!(workflow.contains("pull_request:"));
    assert!(workflow.contains("push:"));
    assert!(workflow.contains("branches:\n      - main"));
    assert!(workflow.contains("runs-on: macos-26"));
    assert!(workflow.contains("timeout-minutes: 30"));
    assert!(workflow.contains("timeout-minutes: 20"));
    assert!(workflow.contains("permissions:\n  contents: read"));
    assert!(workflow.contains("test \"$(uname -m)\" = arm64"));
}

#[test]
fn parser_smoke_checkout_is_sha_pinned_without_credentials() {
    let workflow = workflow();
    assert_eq!(workflow.matches("actions/checkout@").count(), 1);
    assert!(workflow.contains(&format!(
        "uses: {CHECKOUT_PIN} # v4\n        with:\n          persist-credentials: false"
    )));
}

#[test]
fn parser_smoke_has_no_secrets_or_artifact_uploads() {
    let workflow = workflow();
    assert!(!workflow.contains("secrets:"));
    assert!(!workflow.contains("${{ secrets."));
    assert!(!workflow.contains("upload-artifact"));
    assert!(!workflow.contains("actions/upload-artifact"));
}

#[test]
fn parser_smoke_fetches_locked_fuzz_deps_before_offline_smoke() {
    let workflow = workflow();
    let fetch = workflow
        .find("cargo fetch --locked --manifest-path fuzz/Cargo.toml")
        .expect("locked fuzz fetch");
    let smoke = workflow
        .find("./scripts/fuzz-smoke.sh 30")
        .expect("exact smoke command");
    assert!(fetch < smoke);
    assert_eq!(workflow.matches("./scripts/fuzz-smoke.sh 30").count(), 1);
}

#[test]
fn parser_smoke_runs_fuzz_harness_contracts_offline_before_smoke() {
    let workflow = workflow();
    let harness = workflow
        .find("test --manifest-path fuzz/Cargo.toml --test harness_contract --locked --offline")
        .expect("exact fuzz harness contract command");
    let smoke = workflow
        .find("./scripts/fuzz-smoke.sh 30")
        .expect("exact smoke command");
    assert!(workflow.contains("- name: Test fuzz harness contracts"));
    assert!(workflow.contains("RUSTFLAGS=\"--cfg fuzzing\" CARGO_NET_OFFLINE=true"));
    assert!(harness < smoke);
    assert_eq!(
        workflow
            .matches(
                "test --manifest-path fuzz/Cargo.toml --test harness_contract --locked --offline"
            )
            .count(),
        1
    );
}

#[test]
fn parser_smoke_lint_carries_cfg_fuzzing_and_exact_pins() {
    let workflow = workflow();
    assert!(workflow.contains("RUSTFLAGS=\"--cfg fuzzing\""));
    assert!(workflow.contains("nightly-2026-08-29-aarch64-apple-darwin"));
    assert!(workflow.contains("cargo install cargo-fuzz --version 0.13.2 --locked"));
    assert!(!workflow.contains("latest"));
    assert!(!workflow.contains("nightly\n"));
    assert!(workflow
        .contains("Approved pins: nightly-2026-08-29-aarch64-apple-darwin, cargo-fuzz 0.13.2."));
    assert!(!workflow.contains("NEEDS_CURRENT_PIN_SELECTION"));
}

#[test]
fn parser_smoke_is_a_candidate_required_check() {
    let workflow = workflow();
    assert!(workflow.contains("Candidate required check pending separate operator authorization"));
    assert!(workflow.contains("does not make parser-smoke merge-blocking"));
}
