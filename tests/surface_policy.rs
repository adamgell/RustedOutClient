use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

const CHECKOUT_PIN: &str = "actions/checkout@11d5960a326750d5838078e36cf38b85af677262";
const SUPPORTED_GRAPH_COMMAND: &str =
    "cargo tree --locked --target aarch64-apple-darwin --all-features --format '{p}' --prefix none";
const EXACT_AUDIT_COMMAND: &str =
    "cargo audit --ignore RUSTSEC-2026-0194 --ignore RUSTSEC-2026-0195";

fn workflow_run_blocks(workflow: &str) -> Vec<String> {
    let lines = workflow.lines().collect::<Vec<_>>();
    let mut blocks = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        if line.trim() != "run: |" {
            index += 1;
            continue;
        }

        let run_indent = line.len() - line.trim_start().len();
        let content_indent = run_indent + 2;
        index += 1;
        let mut block = Vec::new();
        while index < lines.len() {
            let content = lines[index];
            if content.trim().is_empty() {
                block.push("");
                index += 1;
                continue;
            }
            let indent = content.len() - content.trim_start().len();
            if indent <= run_indent {
                break;
            }
            assert!(
                indent >= content_indent,
                "workflow block has invalid indentation"
            );
            block.push(&content[content_indent..]);
            index += 1;
        }
        blocks.push(block.join("\n"));
    }

    blocks
}

fn advisory_guard_script(workflow: &str) -> String {
    let matching = workflow_run_blocks(workflow)
        .into_iter()
        .filter(|block| block.contains("cargo tree"))
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "workflow must have one supported-graph guard"
    );
    matching.into_iter().next().unwrap()
}

fn native_verification_block(checklist: &str) -> String {
    let matching = checklist
        .split("```bash\n")
        .skip(1)
        .filter_map(|remainder| remainder.split_once("\n```").map(|(block, _)| block))
        .filter(|block| block.contains("cargo tree"))
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "native checklist must have one supported-graph verification block"
    );
    matching.into_iter().next().unwrap().to_owned()
}

#[derive(Clone, Copy)]
enum MatcherMode {
    Working,
    Error,
    Missing,
}

fn run_advisory_guard(script: &str, cargo_mode: &str, matcher_mode: MatcherMode) -> Output {
    let workspace = tempfile::tempdir().unwrap();
    let bin = workspace.path().join("bin");
    let temporary = workspace.path().join("tmp");
    fs::create_dir(&bin).unwrap();
    fs::create_dir(&temporary).unwrap();

    let cargo = bin.join("cargo");
    fs::write(
        &cargo,
        r#"#!/bin/sh
expected='tree --locked --target aarch64-apple-darwin --all-features --format {p} --prefix none'
[ "$*" = "$expected" ] || exit 97
case "$FAKE_CARGO_MODE" in
  fail) exit 42 ;;
  quick-xml) printf '%s\n' 'quick-xml v0.39.4' ;;
  clean) printf '%s\n' 'rustedoutclient v0.1.0' ;;
  *) exit 98 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();

    if !matches!(matcher_mode, MatcherMode::Missing) {
        let matcher = bin.join("rg");
        let matcher_script = match matcher_mode {
            MatcherMode::Working => "#!/bin/sh\nexec /usr/bin/grep \"$@\"\n",
            MatcherMode::Error => "#!/bin/sh\nexit 2\n",
            MatcherMode::Missing => unreachable!(),
        };
        fs::write(&matcher, matcher_script).unwrap();
        fs::set_permissions(&matcher, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let path = if !matches!(matcher_mode, MatcherMode::Missing) {
        env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap()
    } else {
        bin.clone().into_os_string()
    };
    let output = Command::new("/bin/bash")
        .arg("-e")
        .arg("-c")
        .arg(script)
        .env("PATH", path)
        .env("TMPDIR", &temporary)
        .env("FAKE_CARGO_MODE", cargo_mode)
        .output()
        .unwrap();

    assert_eq!(
        fs::read_dir(&temporary).unwrap().count(),
        0,
        "supported graph temporary file must be removed"
    );
    output
}

#[test]
fn product_surface_excludes_removed_features_and_password_cli() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();

    for forbidden in [
        "russh-sftp",
        "rfd =",
        "eax =",
        "aes =",
        "rsa =",
        "sha1 = \"0.10\"",
        "sha2 = \"0.10\"",
        "rand = \"0.8\"",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "forbidden dependency: {forbidden}"
        );
    }
    for removed in ["src/transfer.rs", "src/sessions.rs", "src/protocol/ra2.rs"] {
        assert!(
            !root.join(removed).exists(),
            "removed source remains: {removed}"
        );
    }
    for forbidden in ["--password", "sftp_test", "mod transfer", "mod sessions"] {
        assert!(
            !main.contains(forbidden),
            "forbidden CLI/source surface: {forbidden}"
        );
    }
}

#[test]
fn ci_checkout_is_sha_pinned_without_persisted_credentials() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow = fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();

    assert_eq!(workflow.matches("actions/checkout@").count(), 1);
    assert!(workflow.contains(&format!(
        "uses: {CHECKOUT_PIN} # v4\n        with:\n          persist-credentials: false"
    )));
}

#[test]
fn dependency_advisory_guards_fail_closed_and_precede_the_exact_audit() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow = fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
    let script = advisory_guard_script(&workflow);

    assert!(
        !run_advisory_guard(&script, "fail", MatcherMode::Working)
            .status
            .success(),
        "cargo tree failure must fail the guard"
    );
    let active_match = run_advisory_guard(&script, "quick-xml", MatcherMode::Working);
    assert!(
        !active_match.status.success(),
        "target-active quick-xml must fail the guard"
    );
    assert!(!String::from_utf8_lossy(&active_match.stdout).contains("quick-xml v0.39.4"));
    assert!(
        run_advisory_guard(&script, "clean", MatcherMode::Working)
            .status
            .success(),
        "a clean complete graph must pass the guard"
    );
    assert!(
        !run_advisory_guard(&script, "clean", MatcherMode::Missing)
            .status
            .success(),
        "a missing matcher must fail before policy evaluation"
    );
    let matcher_error = run_advisory_guard(&script, "clean", MatcherMode::Error);
    assert!(
        !matcher_error.status.success(),
        "a present matcher error must fail instead of proving package absence"
    );
    let matcher_error_output = format!(
        "{}{}",
        String::from_utf8_lossy(&matcher_error.stdout),
        String::from_utf8_lossy(&matcher_error.stderr)
    );
    assert!(matcher_error_output.contains("Could not evaluate"));
    assert!(!matcher_error_output.contains("rustedoutclient v0.1.0"));

    let graph_position = workflow
        .find(SUPPORTED_GRAPH_COMMAND)
        .expect("exact supported graph command");
    let audit_position = workflow
        .find(EXACT_AUDIT_COMMAND)
        .expect("exact two-ID audit command");
    assert!(graph_position < audit_position);
    assert_eq!(workflow.matches(EXACT_AUDIT_COMMAND).count(), 1);

    let checklist = fs::read_to_string(root.join("docs/native-acceptance.md")).unwrap();
    let block = native_verification_block(&checklist);

    assert!(
        block.contains("command -v rg"),
        "matcher preflight is required"
    );
    assert!(
        block.contains("graph_file=\"$(mktemp") && block.contains(")\" || {"),
        "temporary graph creation must fail closed"
    );
    assert!(block.contains("trap 'rm -f \"$graph_file\"' EXIT"));
    assert!(block.contains("matcher_status=$?"));
    assert!(block.contains("case \"$matcher_status\" in"));
    assert!(block.contains("  0)"));
    assert!(block.contains("  1)"));
    assert!(block.contains("  *)"));
    assert!(block.contains("if ! rm -f \"$graph_file\"; then"));
    assert!(block.contains("trap - EXIT"));

    let graph_position = block
        .find(SUPPORTED_GRAPH_COMMAND)
        .expect("exact supported graph command");
    let cleanup_position = block.find("trap - EXIT").expect("cleared cleanup trap");
    let audit_position = block
        .find(EXACT_AUDIT_COMMAND)
        .expect("exact two-ID audit command");
    assert!(graph_position < cleanup_position);
    assert!(cleanup_position < audit_position);
}
