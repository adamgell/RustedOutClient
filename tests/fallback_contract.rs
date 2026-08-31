use std::{
    fs,
    path::{Path, PathBuf},
};

use rustedoutclient::{
    fallback::{FallbackError, FallbackPreferences, FallbackSession, TigerVncFallback},
    runtime::RuntimeDir,
    ssh::TrustedSshProxy,
};

// Compiling this helper pins the only production fallback constructor to the
// verified SSH transport, private runtime, configured path, and two display
// preferences. It is deliberately never called by this synthetic test suite.
#[allow(dead_code)]
async fn production_open_surface(
    proxy: TrustedSshProxy,
    runtime: &RuntimeDir,
    viewer: &Path,
    preferences: FallbackPreferences,
) -> Result<FallbackSession, FallbackError> {
    TigerVncFallback::open(proxy, runtime, viewer, preferences).await
}

fn source(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path)).unwrap()
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_owned()];
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files.sort();
    files
}

fn production_portion<'a>(relative_path: &Path, text: &'a str) -> &'a str {
    const TEST_MODULE_MARKER: &str = "\n#[cfg(test)]\nmod tests {";

    let test_module_declarations = text
        .lines()
        .filter(|line| {
            line.chars()
                .filter(|character| !character.is_ascii_whitespace())
                .collect::<String>()
                .ends_with("modtests{")
        })
        .count();
    let exact_markers = text.matches(TEST_MODULE_MARKER).count();
    if test_module_declarations == 0 {
        assert_eq!(
            exact_markers,
            0,
            "{} has an ambiguous test-module marker",
            relative_path.display()
        );
        return text;
    }

    assert_eq!(
        test_module_declarations,
        1,
        "{} has multiple conventional test modules",
        relative_path.display()
    );
    assert_eq!(
        exact_markers,
        1,
        "{} has an unsupported cfg(test) module layout",
        relative_path.display()
    );
    let (production, test_module) = text
        .split_once(TEST_MODULE_MARKER)
        .expect("the exact marker count was checked");
    let mut consumed = 0;
    let terminal_module_end = test_module
        .split_inclusive('\n')
        .find_map(|line| {
            consumed += line.len();
            (line.trim_end_matches(['\r', '\n']) == "}").then_some(consumed)
        })
        .unwrap_or_else(|| {
            panic!(
                "{} has an unterminated conventional test module",
                relative_path.display()
            )
        });
    assert!(
        test_module[terminal_module_end..].trim().is_empty(),
        "{} has source after its conventional test module",
        relative_path.display()
    );
    production
}

#[test]
fn public_surface_is_verified_transport_only_and_contains_no_dangerous_constructor() {
    let fallback = source("src/fallback/mod.rs");
    let public_prefix = fallback
        .split("\n#[cfg(test)]\nmod tests")
        .next()
        .expect("fallback source has a production section");

    assert!(public_prefix.contains("pub async fn open("));
    assert!(public_prefix.contains("TrustedSshProxy"));
    assert!(public_prefix.contains("&RuntimeDir"));
    assert!(public_prefix.contains("&Path"));
    assert!(public_prefix.contains("FallbackPreferences"));
    for forbidden in [
        "bind_addr",
        "bind_address",
        "clear_password",
        "password: String",
        "host: String",
        "endpoint: String",
        "VncCommand",
        "TcpStream",
        "dyn AsyncRead",
        "auto_fallback",
    ] {
        assert!(
            !public_prefix.contains(forbidden),
            "dangerous public fallback surface contains {forbidden}"
        );
    }
}

#[test]
fn listener_process_and_password_artifacts_stay_inside_the_approved_boundary() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let relay = source("src/fallback/relay.rs");
    let password = source("src/fallback/password_file.rs");
    let fallback = source("src/fallback/mod.rs");
    let viewer = source("src/fallback/viewer.rs");
    let manager = source("src/session/manager.rs");
    assert!(relay.contains("TcpListener::bind((Ipv4Addr::LOCALHOST, 0))"));
    assert!(relay.contains("copy_bidirectional"));
    assert!(viewer.contains("File::open(configured_path)"));
    assert!(viewer.contains(".metadata()"));
    assert!(viewer.contains("create_new(true)"));
    assert!(fallback.contains("tokio::process::Command::new(snapshot.path())"));
    assert!(!fallback.contains("tokio::process::Command::new(viewer_path)"));
    assert!(password.contains("0xE8, 0x4A, 0xD6, 0x60, 0xC4, 0x72, 0x1A, 0xE0"));

    const APPROVED_RELAY: &str = "src/fallback/relay.rs";
    const APPROVED_VIEWER_ENDPOINT: &str = "src/fallback/mod.rs";
    const APPROVED_BIND: &str = "TcpListener::bind((Ipv4Addr::LOCALHOST, 0))";
    const SOCKET_SURFACES: [&str; 7] = [
        "TcpListener",
        "TcpStream",
        "TcpSocket",
        "UdpSocket",
        "UnixListener",
        "UnixStream",
        "UnixDatagram",
    ];
    let mut approved_relay_seen = false;
    for file in rust_sources(&manifest.join("src")) {
        let relative_path = file
            .strip_prefix(manifest)
            .expect("enumerated source remains below the manifest root");
        let text = fs::read_to_string(&file).unwrap();
        let production = production_portion(relative_path, &text);
        let compact = production
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect::<String>();
        let bind_calls = compact.matches("bind(").count() + compact.matches("bind::<").count();

        if relative_path == Path::new(APPROVED_RELAY) {
            approved_relay_seen = true;
            assert_eq!(
                production.matches(APPROVED_BIND).count(),
                1,
                "approved relay must contain one exact loopback bind"
            );
            assert_eq!(
                bind_calls, 1,
                "approved relay gained a second or non-approved bind"
            );
            for forbidden in &SOCKET_SURFACES[2..] {
                assert!(
                    !production.contains(forbidden),
                    "approved relay gained another socket family: {forbidden}"
                );
            }
        } else {
            for forbidden in SOCKET_SURFACES {
                assert!(
                    !production.contains(forbidden),
                    "{} gained a listener/socket surface: {forbidden}",
                    relative_path.display()
                );
            }
            assert_eq!(
                bind_calls,
                0,
                "{} gained a production bind",
                relative_path.display()
            );
        }
        if relative_path != Path::new(APPROVED_VIEWER_ENDPOINT) {
            assert!(
                !production.contains("127.0.0.1"),
                "{} gained an unapproved loopback endpoint",
                relative_path.display()
            );
        }
    }
    assert!(
        approved_relay_seen,
        "approved relay source was not enumerated"
    );

    let cleanup_cycle = manager
        .split("async fn ten_connect_disconnect_cycles_leave_zero_exact_owned_residue()")
        .nth(1)
        .expect("Task 13 cleanup cycle exists")
        .split("\n    #[cfg(unix)]")
        .next()
        .expect("Task 13 cleanup cycle has a bounded source section");
    for unrelated in ["TcpListener", "listener_task", "rebound", "drop(runtime)"] {
        assert!(
            !cleanup_cycle.contains(unrelated),
            "cleanup proof retained unrelated listener evidence: {unrelated}"
        );
    }
}

#[test]
fn launch_and_lifecycle_constants_are_fixed_and_secret_free() {
    let fallback_source = source("src/fallback/mod.rs");
    let fallback = fallback_source
        .split("\n#[cfg(test)]\nmod tests")
        .next()
        .unwrap();
    let relay = source("src/fallback/relay.rs");
    for argument in [
        "-Shared=1",
        "-RemoteResize=1",
        "-SecurityTypes=VncAuth",
        "-PasswordFile",
        "-FullScreen=1",
        "-ViewOnly=1",
    ] {
        assert!(
            fallback.contains(argument),
            "missing fixed argument {argument}"
        );
    }
    assert!(fallback.contains(".env_clear()"));
    assert!(fallback.contains(".env(\"PATH\", \"/usr/bin:/bin\")"));
    for allowed in ["HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE"] {
        assert!(fallback.contains(allowed));
    }
    assert!(relay.contains("Duration::from_secs(20)"));
    assert!(fallback.contains("Duration::from_secs(3)"));
    assert!(!fallback.contains("Stdio::piped"));
    assert!(!fallback.contains("/bin/sh"));
    assert!(!fallback.contains("sh -c"));
}

#[test]
fn semantic_command_contains_only_target_and_display_preferences() {
    let events = source("src/session/events.rs");
    let command = events
        .split("OpenInTigerVnc")
        .nth(1)
        .expect("semantic fallback command exists")
        .split('}')
        .next()
        .unwrap();
    assert!(command.contains("vmid: VmId"));
    assert!(command.contains("preferences: FallbackPreferences"));
    for forbidden in [
        "PathBuf",
        "String",
        "password",
        "endpoint",
        "host",
        "VncCommand",
        "TrustedSshProxy",
    ] {
        assert!(!command.contains(forbidden));
    }
}

#[test]
fn production_manager_rechecks_inventory_before_one_pinned_viewer_open() {
    let manager = source("src/session/manager.rs");
    let production = manager
        .split("impl SessionBackend for ProductionBackend")
        .nth(1)
        .unwrap();
    let fallback = production
        .split("fn open_fallback(")
        .nth(1)
        .unwrap()
        .split("fn close_master")
        .next()
        .unwrap();
    assert!(!fallback.contains("validate_viewer_path"));
    let verify = fallback.find("master.verify()").unwrap();
    let connect = fallback.find("TrustedSshProxy::connect").unwrap();
    let open = fallback.find("TigerVncFallback::open").unwrap();
    assert!(verify < connect && connect < open);

    let native_error_mapping = manager
        .split("fn public_rfb_error")
        .nth(1)
        .unwrap()
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert!(!native_error_mapping.contains("OpenInTigerVnc"));
}

#[test]
fn pure_state_retains_only_configuration_presence_and_shutdown_closes_fallbacks_first() {
    let state = source("src/app/state.rs");
    let fields = state
        .split("pub struct AppState")
        .nth(1)
        .unwrap()
        .split("impl AppState")
        .next()
        .unwrap();
    assert!(fields.contains("fallback_configured: bool"));
    assert!(!fields.contains("fallback_viewer"));
    assert!(!fields.contains("PathBuf"));

    let manager = source("src/session/manager.rs");
    let shutdown = manager
        .split("async fn shutdown(&mut self)")
        .nth(1)
        .unwrap()
        .split("fn active_session_count")
        .next()
        .unwrap();
    assert!(
        shutdown.find("record.session.close()").unwrap()
            < shutdown.find("self.backend.close_master()").unwrap()
    );
}
