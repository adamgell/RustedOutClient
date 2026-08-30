use std::{ffi::OsString, path::PathBuf};

use rustedoutclient::{
    model::{NodeName, PveProfile, SshTarget, VmId},
    ssh::{classify_stderr, CommandSpec, ProxyTicket, SshCommandFactory, SshFailureKind},
};

const SOCKET: &str = "/tmp/roc-test/c";
const TARGET: &str = "root@pve.example.invalid";

fn fixture_profile() -> PveProfile {
    PveProfile {
        name: "Test Proxmox".to_owned(),
        ssh_target: SshTarget::parse(TARGET).unwrap(),
        node: NodeName::parse("pve2").unwrap(),
    }
}

fn fixture_factory() -> SshCommandFactory {
    SshCommandFactory::new(PathBuf::from(SOCKET))
}

fn os_args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn common_args() -> Vec<&'static str> {
    vec![
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=12",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "PasswordAuthentication=no",
        "-o",
        "KbdInteractiveAuthentication=no",
    ]
}

fn assert_common_contract(spec: &CommandSpec) {
    assert_eq!(spec.program, PathBuf::from("/usr/bin/ssh"));
    assert!(spec.capture_stderr);
    assert_eq!(spec.environment_variable_count(), 0);

    for option in [
        "BatchMode=yes",
        "ConnectTimeout=12",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
        "StrictHostKeyChecking=yes",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
    ] {
        assert!(
            spec.args
                .windows(2)
                .any(|window| window[0] == "-o" && window[1] == option),
            "missing fixed SSH option: {option}"
        );
    }

    assert_eq!(spec.args.iter().filter(|arg| *arg == TARGET).count(), 1);
    assert!(!spec
        .args
        .iter()
        .any(|arg| { matches!(arg.to_str(), Some("sh" | "bash" | "zsh" | "fish" | "-c")) }));
}

#[test]
fn every_operation_has_exact_strict_shell_free_argv() {
    let profile = fixture_profile();
    let factory = fixture_factory();

    let mut master = common_args();
    master.extend(["-M", "-N", "-o", "ControlPersist=no", "-S", SOCKET, TARGET]);
    let mut check = common_args();
    check.extend(["-S", SOCKET, "-O", "check", TARGET]);
    let mut exit = common_args();
    exit.extend(["-S", SOCKET, "-O", "exit", TARGET]);
    let mut inventory = common_args();
    inventory.extend([
        "-S",
        SOCKET,
        TARGET,
        "pvesh get /nodes/pve2/qemu --output-format json",
    ]);
    for (spec, expected) in [
        (factory.master(&profile), master),
        (factory.check(&profile), check),
        (factory.exit(&profile), exit),
        (factory.inventory(&profile), inventory),
    ] {
        let spec = spec.unwrap();
        assert_common_contract(&spec);
        assert_eq!(spec.args, os_args(&expected));
    }

    let ticket = ProxyTicket::generate();
    let proxy = factory
        .proxy(&profile, VmId::new(107).unwrap(), &ticket)
        .unwrap();
    let mut expected_proxy = common_args();
    expected_proxy.extend([
        "-o",
        "SendEnv=LC_PVE_TICKET",
        "-S",
        SOCKET,
        TARGET,
        "exec /usr/sbin/qm vncproxy 107",
    ]);
    assert_eq!(proxy.program, PathBuf::from("/usr/bin/ssh"));
    assert!(proxy.capture_stderr);
    assert_eq!(proxy.environment_variable_count(), 1);
    assert_eq!(proxy.args, os_args(&expected_proxy));
}

#[test]
fn inventory_and_proxy_keep_the_allowlisted_remote_commands_as_single_arguments() {
    let profile = fixture_profile();
    let factory = fixture_factory();

    let inventory = factory.inventory(&profile).unwrap();
    assert_eq!(
        inventory.args.last().unwrap(),
        "pvesh get /nodes/pve2/qemu --output-format json"
    );

    let ticket = ProxyTicket::generate();
    let proxy = factory
        .proxy(&profile, VmId::new(107).unwrap(), &ticket)
        .unwrap();
    assert_eq!(proxy.args.last().unwrap(), "exec /usr/sbin/qm vncproxy 107");
    assert_eq!(
        proxy
            .args
            .iter()
            .filter(|arg| *arg == "SendEnv=LC_PVE_TICKET")
            .count(),
        1
    );
}

#[test]
fn bounded_stderr_classification_distinguishes_safe_public_failure_kinds() {
    let cases = [
        (
            "No ED25519 host key is known for pve.example.invalid and you have requested strict checking.\nHost key verification failed.",
            SshFailureKind::HostKeyUnknown,
        ),
        (
            "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!\nOffending ED25519 key in /Users/test/.ssh/known_hosts:4",
            SshFailureKind::HostKeyChanged,
        ),
        (
            "root@pve.example.invalid: Permission denied (publickey,keyboard-interactive).",
            SshFailureKind::Authentication,
        ),
        (
            "ssh: connect to host pve.example.invalid port 22: Operation timed out",
            SshFailureKind::Timeout,
        ),
        (
            "ssh: connect to host pve.example.invalid port 65535: Connection timed out",
            SshFailureKind::Timeout,
        ),
        ("ssh: connect to host pve.example.invalid port 22: No route to host", SshFailureKind::Ssh),
        (
            "pvesh get: Permission denied while reading /nodes/pve2/qemu",
            SshFailureKind::Ssh,
        ),
        (
            "qm vncproxy: Operation timed out while waiting for the guest",
            SshFailureKind::Ssh,
        ),
        ("qm: Permission denied (publickey).", SshFailureKind::Ssh),
        (
            "ssh: connect to host pve.example.invalid port invalid: Operation timed out",
            SshFailureKind::Ssh,
        ),
        (
            "ssh: connect to host pve.example.invalid port 0: Operation timed out",
            SshFailureKind::Ssh,
        ),
        (
            "ssh: connect to host pve.example.invalid port 65536: Operation timed out",
            SshFailureKind::Ssh,
        ),
    ];

    for (stderr, expected) in cases {
        assert_eq!(classify_stderr(stderr.as_bytes()).kind(), expected);
    }
}

#[test]
fn classification_bounds_stderr_before_matching_or_public_rendering() {
    let target = "root@pve.example.invalid";
    let fingerprint = "SHA256:do-not-display";
    let mut stderr = vec![b'x'; 65_536];
    stderr.extend_from_slice(
        format!(" {target} {fingerprint} Permission denied (publickey).",).as_bytes(),
    );

    let failure = classify_stderr(&stderr);
    assert_eq!(failure.kind(), SshFailureKind::Ssh);

    let public_display = failure.to_string();
    let public_debug = format!("{failure:?}");
    for rendered in [public_display, public_debug] {
        assert!(!rendered.contains(target));
        assert!(!rendered.contains(fingerprint));
        assert!(!rendered.contains("Permission denied"));
    }
}
