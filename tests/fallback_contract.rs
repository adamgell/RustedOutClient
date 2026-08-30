use std::{fs, path::Path};

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
    let relay = source("src/fallback/relay.rs");
    let password = source("src/fallback/password_file.rs");
    let fallback = source("src/fallback/mod.rs");
    let viewer = source("src/fallback/viewer.rs");
    assert!(relay.contains("TcpListener::bind((Ipv4Addr::LOCALHOST, 0))"));
    assert!(relay.contains("copy_bidirectional"));
    assert!(viewer.contains("File::open(configured_path)"));
    assert!(viewer.contains(".metadata()"));
    assert!(viewer.contains("create_new(true)"));
    assert!(fallback.contains("tokio::process::Command::new(snapshot.path())"));
    assert!(!fallback.contains("tokio::process::Command::new(viewer_path)"));
    assert!(password.contains("0xE8, 0x4A, 0xD6, 0x60, 0xC4, 0x72, 0x1A, 0xE0"));

    for native in ["src/vnc", "src/connection.rs", "src/session/events.rs"] {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(native);
        let files = if root.is_dir() {
            fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
                .collect::<Vec<_>>()
        } else {
            vec![root]
        };
        for file in files {
            let text = fs::read_to_string(&file).unwrap();
            assert!(
                !text.contains("TcpListener"),
                "native source bound a listener"
            );
            assert!(
                !text.contains("127.0.0.1::"),
                "native source gained an endpoint"
            );
        }
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
fn production_manager_validates_then_rechecks_inventory_before_fresh_proxy_open() {
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
    let validate = fallback.find("validate_viewer_path").unwrap();
    let verify = fallback.find("master.verify()").unwrap();
    let connect = fallback.find("TrustedSshProxy::connect").unwrap();
    let open = fallback.find("TigerVncFallback::open").unwrap();
    assert!(validate < verify && verify < connect && connect < open);

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
