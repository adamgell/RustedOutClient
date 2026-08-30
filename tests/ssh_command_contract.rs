use std::{ffi::OsString, path::PathBuf};

use rustedoutclient::{
    model::{NodeName, PveProfile, SshTarget, VmId},
    ssh::{classify_stderr, CommandSpec, SshCommandFactory, SshFailureKind},
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
    assert!(spec.env.is_empty());

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
    let mut proxy = common_args();
    proxy.extend(["-S", SOCKET, TARGET, "exec /usr/sbin/qm vncproxy 107"]);

    for (spec, expected) in [
        (factory.master(&profile), master),
        (factory.check(&profile), check),
        (factory.exit(&profile), exit),
        (factory.inventory(&profile), inventory),
        (factory.proxy(&profile, VmId::new(107).unwrap()), proxy),
    ] {
        let spec = spec.unwrap();
        assert_common_contract(&spec);
        assert_eq!(spec.args, os_args(&expected));
    }
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

    let proxy = factory.proxy(&profile, VmId::new(107).unwrap()).unwrap();
    assert_eq!(proxy.args.last().unwrap(), "exec /usr/sbin/qm vncproxy 107");
}

#[test]
fn command_spec_builds_the_direct_ssh_process_without_an_interpreter() {
    let spec = fixture_factory().inventory(&fixture_profile()).unwrap();
    let command = spec.to_command();

    assert_eq!(command.get_program(), PathBuf::from("/usr/bin/ssh"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        spec.args
            .iter()
            .map(OsString::as_os_str)
            .collect::<Vec<_>>()
    );
    assert_eq!(command.get_envs().count(), 0);
}

#[test]
fn test_owned_executable_seam_is_explicit_and_does_not_change_production_construction() {
    let injected = PathBuf::from("/private/test-fixtures/fake-ssh");
    let test_spec = SshCommandFactory::new_for_test(injected.clone(), PathBuf::from(SOCKET))
        .inventory(&fixture_profile())
        .unwrap();
    let production_spec = fixture_factory().inventory(&fixture_profile()).unwrap();

    assert_eq!(test_spec.program, injected);
    assert_eq!(production_spec.program, PathBuf::from("/usr/bin/ssh"));
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
        ("root@pve.example.invalid: Permission denied (publickey).", SshFailureKind::Authentication),
        ("ssh: connect to host pve.example.invalid port 22: Operation timed out", SshFailureKind::Timeout),
        ("ssh: connect to host pve.example.invalid port 22: No route to host", SshFailureKind::Ssh),
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
