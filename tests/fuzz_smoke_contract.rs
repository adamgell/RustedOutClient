use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::Duration,
};

const TARGETS: [&str; 5] = [
    "rfb_handshake",
    "rfb_session",
    "rfb_zrle",
    "rfb_tight",
    "rfb_hextile",
];

struct Harness {
    root: tempfile::TempDir,
    bin: PathBuf,
    tmp: PathBuf,
    log: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        let tmp = root.path().join("tmp");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&tmp).unwrap();
        fs::create_dir_all(root.path().join("scripts")).unwrap();
        fs::create_dir_all(root.path().join("fuzz/fuzz_targets")).unwrap();
        for target in TARGETS {
            fs::create_dir_all(root.path().join("fuzz/corpus").join(target)).unwrap();
            fs::write(
                root.path()
                    .join("fuzz/fuzz_targets")
                    .join(format!("{target}.rs")),
                "fn target() {}\n",
            )
            .unwrap();
        }
        fs::write(
            root.path().join("fuzz/Cargo.toml"),
            "[package]\nname = \"fuzz\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(root.path().join("fuzz/Cargo.lock"), "# lock\n").unwrap();
        fs::write(
            root.path().join("fuzz/rust-toolchain.toml"),
            "[toolchain]\nchannel = \"nightly-2026-08-29-aarch64-apple-darwin\"\n",
        )
        .unwrap();
        fs::write(
            root.path().join("fuzz/corpus-manifest.json"),
            r#"{"version":1,"targets":["rfb_handshake","rfb_session","rfb_zrle","rfb_tight","rfb_hextile"],"seeds":[]}"#,
        )
        .unwrap();

        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/fuzz-smoke.sh");
        fs::copy(&source, root.path().join("scripts/fuzz-smoke.sh")).unwrap();
        fs::set_permissions(
            root.path().join("scripts/fuzz-smoke.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let log = root.path().join("fake.log");
        fs::write(&log, "").unwrap();
        Self {
            root,
            bin,
            tmp,
            log,
        }
    }

    fn write_exec(&self, name: &str, body: &str) {
        let path = self.bin.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn install_ok_tools(&self) {
        self.write_exec(
            "cargo-fuzz",
            "#!/bin/sh\necho 'cargo-fuzz should not be invoked directly' >&2\nexit 97\n",
        );
        self.write_exec(
            "rustup",
            r#"#!/bin/sh
[ "$1" = toolchain ] && [ "$2" = list ] || exit 97
printf '%s\n' 'nightly-2026-08-29-aarch64-apple-darwin (default)'
"#,
        );
        let log = self.log.display();
        self.write_exec(
            "cargo",
            &format!(
                r#"#!/bin/sh
set -u
log="{log}"
if [ "$#" -gt 0 ]; then
  case "$1" in
    +*) shift ;;
  esac
fi
printf '%s\n' "$*" >> "$log"
case "$*" in
  "check --locked --offline --manifest-path fuzz/Cargo.toml --lib")
    if [ "${{FAKE_FETCH_FAIL:-0}}" = 1 ]; then
      exit 1
    fi
    exit 0
    ;;
  "fmt --manifest-path fuzz/Cargo.toml -- --check")
    exit 0
    ;;
  "run --manifest-path fuzz/Cargo.toml --bin verify_seeds --locked --offline")
    if [ "${{FAKE_VERIFY_FAIL:-0}}" = 1 ]; then
      exit 3
    fi
    exit 0
    ;;
  fuzz\ run\ *)
    count=$(grep -c '^fuzz run ' "$log" || true)
    if [ "${{FAKE_FUZZ_SLEEP:-0}}" = 1 ]; then
      sleep 30
      exit 0
    fi
    if [ "${{FAKE_FUZZ_FAIL_AT:-}}" != "" ] && [ "$count" -eq "${{FAKE_FUZZ_FAIL_AT}}" ]; then
      exit "${{FAKE_FUZZ_FAIL_STATUS:-42}}"
    fi
    exit 0
    ;;
  *)
    exit 97
    ;;
esac
"#
            ),
        );
    }

    fn path(&self) -> std::ffi::OsString {
        env::join_paths([
            self.bin.as_path(),
            Path::new("/usr/bin"),
            Path::new("/bin"),
            Path::new("/usr/sbin"),
        ])
        .unwrap()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(self.root.path().join("scripts/fuzz-smoke.sh"));
        command
            .current_dir(self.root.path())
            .env("PATH", self.path())
            .env("TMPDIR", &self.tmp)
            .args(args);
        command
    }

    fn fuzz_invocations(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("fuzz run "))
            .map(str::to_string)
            .collect()
    }

    fn tmp_empty(&self) -> bool {
        fs::read_dir(&self.tmp).unwrap().next().is_none()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn missing_cargo_fuzz_exits_before_any_target_run() {
    let harness = Harness::new();
    harness.install_ok_tools();
    fs::remove_file(harness.bin.join("cargo-fuzz")).unwrap();
    let output = harness.command(&["30"]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(harness.fuzz_invocations().is_empty());
    assert!(harness.tmp_empty());
}

#[test]
fn invalid_durations_print_usage_and_exit_2() {
    let harness = Harness::new();
    harness.install_ok_tools();
    for args in [&[][..], &["0"], &["-5"], &["abc"], &["30", "1"]] {
        let output = harness.command(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "args={args:?}");
        assert!(stderr(&output).contains("usage: ./scripts/fuzz-smoke.sh <positive-seconds>"));
        assert!(harness.fuzz_invocations().is_empty());
    }
}

#[test]
fn target_list_drift_exits_before_any_run() {
    let harness = Harness::new();
    harness.install_ok_tools();

    fs::write(
        harness.root.path().join("fuzz/fuzz_targets/rfb_extra.rs"),
        "fn extra() {}\n",
    )
    .unwrap();
    let stray = harness.command(&["30"]).output().unwrap();
    assert_eq!(stray.status.code(), Some(1));
    assert!(harness.fuzz_invocations().is_empty());
    fs::remove_file(harness.root.path().join("fuzz/fuzz_targets/rfb_extra.rs")).unwrap();

    fs::remove_file(harness.root.path().join("fuzz/fuzz_targets/rfb_hextile.rs")).unwrap();
    let missing = harness.command(&["30"]).output().unwrap();
    assert_eq!(missing.status.code(), Some(1));
    assert!(harness.fuzz_invocations().is_empty());
    fs::write(
        harness.root.path().join("fuzz/fuzz_targets/rfb_hextile.rs"),
        "fn target() {}\n",
    )
    .unwrap();

    fs::write(
        harness.root.path().join("fuzz/corpus-manifest.json"),
        r#"{"version":1,"targets":["rfb_handshake"],"seeds":[]}"#,
    )
    .unwrap();
    let drifted = harness.command(&["30"]).output().unwrap();
    assert_eq!(drifted.status.code(), Some(1));
    assert!(harness.fuzz_invocations().is_empty());
}

#[test]
fn uncached_dependencies_print_guidance_and_skip_targets() {
    let harness = Harness::new();
    harness.install_ok_tools();
    let output = harness
        .command(&["30"])
        .env("FAKE_FETCH_FAIL", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(text.contains("cargo fetch --locked --manifest-path fuzz/Cargo.toml"));
    assert!(harness.fuzz_invocations().is_empty());
    assert!(harness.tmp_empty());
}

#[test]
fn seed_verification_failure_skips_fuzz_runs() {
    let harness = Harness::new();
    harness.install_ok_tools();
    let output = harness
        .command(&["30"])
        .env("FAKE_VERIFY_FAIL", "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(harness.fuzz_invocations().is_empty());
    assert!(harness.tmp_empty());
}

#[test]
fn child_failure_stops_on_third_target_and_removes_temp() {
    let harness = Harness::new();
    harness.install_ok_tools();
    let output = harness
        .command(&["30"])
        .env("FAKE_FUZZ_FAIL_AT", "3")
        .env("FAKE_FUZZ_FAIL_STATUS", "42")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42));
    let invocations = harness.fuzz_invocations();
    assert_eq!(invocations.len(), 3);
    assert!(invocations[0].contains("rfb_handshake"));
    assert!(invocations[1].contains("rfb_session"));
    assert!(invocations[2].contains("rfb_zrle"));
    let combined = stdout(&output);
    assert!(combined.contains("FAIL rfb_zrle"));
    assert!(!combined.contains("PASS rfb_tight"));
    assert!(!combined.contains("FAIL rfb_tight"));
    assert!(harness.tmp_empty());
}

#[test]
fn interruption_removes_temp_dir() {
    let harness = Harness::new();
    harness.install_ok_tools();
    let mut child = harness
        .command(&["30"])
        .env("FAKE_FUZZ_SLEEP", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    let _ = Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
    thread::sleep(Duration::from_millis(150));
    assert!(harness.tmp_empty());
}

#[test]
fn successful_sequencing_runs_five_targets_and_removes_temp() {
    let harness = Harness::new();
    harness.install_ok_tools();
    let output = harness.command(&["30"]).output().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        stdout(&output),
        stderr(&output)
    );
    let invocations = harness.fuzz_invocations();
    assert_eq!(invocations.len(), 5, "{invocations:?}");
    for (index, target) in TARGETS.iter().enumerate() {
        assert!(
            invocations[index].contains(&format!("fuzz run {target} ")),
            "{}",
            invocations[index]
        );
        assert!(
            invocations[index].contains(&format!("/corpus/{target}")),
            "{}",
            invocations[index]
        );
        assert!(
            !invocations[index].contains("fuzz/corpus/"),
            "{}",
            invocations[index]
        );
        assert!(
            invocations[index].contains("-max_total_time=30"),
            "{}",
            invocations[index]
        );
    }

    let combined = stdout(&output);
    for target in TARGETS {
        assert!(combined.contains(&format!("PASS {target}")));
    }
    assert!(combined.contains("fuzz-smoke: 5/5 targets passed"));
    assert!(harness.tmp_empty());
}

#[test]
fn successful_run_leaves_source_corpus_untouched() {
    let harness = Harness::new();
    harness.install_ok_tools();
    let output = harness.command(&["30"]).output().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        stdout(&output),
        stderr(&output)
    );
    for target in TARGETS {
        let dir = harness.root.path().join("fuzz/corpus").join(target);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            0,
            "source corpus {target} must stay empty"
        );
    }
    assert!(harness.tmp_empty());
}
