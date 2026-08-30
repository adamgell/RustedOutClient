use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use rustedoutclient::{
    config::{
        import_legacy_json, load_config_from_path, save_config_to_path,
        save_config_to_path_with_renamer, AppConfig, AtomicRenamer,
    },
    model::{NodeName, PveProfile, SshTarget, VmId},
};
use tempfile::{tempdir, TempDir};

fn test_directory() -> TempDir {
    let directory = tempdir().unwrap();
    #[cfg(unix)]
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

#[test]
fn concurrent_test_directories_are_unique_and_owned() {
    use std::{
        collections::HashSet,
        sync::{Arc, Barrier},
        thread,
    };

    const WORKERS: usize = 256;
    let barrier = Arc::new(Barrier::new(WORKERS));
    let handles: Vec<_> = (0..WORKERS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                test_directory()
            })
        })
        .collect();

    let mut directories = Vec::with_capacity(WORKERS);
    let mut panicked = false;
    for handle in handles {
        match handle.join() {
            Ok(directory) => directories.push(directory),
            Err(_) => panicked = true,
        }
    }

    let unique_paths: HashSet<_> = directories
        .iter()
        .map(|directory| directory.path().to_owned())
        .collect();

    assert!(!panicked, "a concurrent directory creation panicked");
    assert_eq!(unique_paths.len(), WORKERS);
}

fn fixture_config() -> AppConfig {
    AppConfig::new(PveProfile {
        name: "Test Proxmox".to_owned(),
        ssh_target: SshTarget::parse("root@example.invalid").unwrap(),
        node: NodeName::parse("pve2").unwrap(),
    })
}

struct FailingRenamer;

impl AtomicRenamer for FailingRenamer {
    fn rename(&self, _source: &Path, _destination: &Path) -> io::Result<()> {
        Err(io::Error::other("synthetic rename failure"))
    }
}

#[test]
fn schema_rejects_shell_like_or_secret_bearing_values() {
    assert!(SshTarget::parse("-oProxyCommand=bad").is_err());
    assert!(SshTarget::parse("root@example.invalid extra").is_err());
    assert!(NodeName::parse("pve2;id").is_err());
    assert!(VmId::new(99).is_err());
    assert!(VmId::new(100_000_000).is_err());
}

#[test]
fn legacy_import_contains_only_non_secret_fields() {
    let imported = import_legacy_json(
        r#"{"ssh_target":"root@example.invalid","node":"pve2","viewer":"/opt/homebrew/bin/vncviewer","password":"must-not-import"}"#,
    )
    .unwrap();
    let json = serde_json::to_value(imported).unwrap();

    assert!(json.get("password").is_none());
    assert_eq!(json["profile"]["node"], "pve2");
}

#[test]
fn schema_rejects_refresh_values_outside_the_safe_interval() {
    let mut config = fixture_config();
    config.inventory_refresh_seconds = 4;
    assert!(config.validate().is_err());

    config.inventory_refresh_seconds = 301;
    assert!(config.validate().is_err());
}

#[test]
fn serde_rejects_persisted_config_with_an_unsupported_schema_version() {
    let json = r#"{
        "schema_version": 2,
        "profile": {"name": "Test", "ssh_target": "root@example.invalid", "node": "pve2"},
        "inventory_refresh_seconds": 15,
        "fallback_viewer": null,
        "clipboard_enabled": false,
        "favorites": [],
        "display": {"scale_mode": "fit", "view_only": false}
    }"#;

    assert!(serde_json::from_str::<AppConfig>(json).is_err());
}

#[test]
fn serde_rejects_persisted_config_with_an_out_of_range_refresh_interval() {
    for json in [
        r#"{
            "schema_version": 1,
            "profile": {"name": "Test", "ssh_target": "root@example.invalid", "node": "pve2"},
            "inventory_refresh_seconds": 4,
            "fallback_viewer": null,
            "clipboard_enabled": false,
            "favorites": [],
            "display": {"scale_mode": "fit", "view_only": false}
        }"#,
        r#"{
            "schema_version": 1,
            "profile": {"name": "Test", "ssh_target": "root@example.invalid", "node": "pve2"},
            "inventory_refresh_seconds": 301,
            "fallback_viewer": null,
            "clipboard_enabled": false,
            "favorites": [],
            "display": {"scale_mode": "fit", "view_only": false}
        }"#,
    ] {
        assert!(serde_json::from_str::<AppConfig>(json).is_err());
    }
}

#[test]
fn serde_rejects_persisted_config_with_a_relative_viewer_path() {
    let json = r#"{
        "schema_version": 1,
        "profile": {"name": "Test", "ssh_target": "root@example.invalid", "node": "pve2"},
        "inventory_refresh_seconds": 15,
        "fallback_viewer": "vncviewer",
        "clipboard_enabled": false,
        "favorites": [],
        "display": {"scale_mode": "fit", "view_only": false}
    }"#;

    assert!(serde_json::from_str::<AppConfig>(json).is_err());
}

#[test]
fn schema_rejects_relative_fallback_viewer_paths() {
    let mut config = fixture_config();
    config.fallback_viewer = Some(PathBuf::from("vncviewer"));

    assert!(config.validate().is_err());
}

#[test]
fn new_configuration_disables_clipboard_by_default() {
    assert!(!fixture_config().clipboard_enabled);
}

#[cfg(unix)]
#[test]
fn atomic_save_keeps_previous_file_readable_when_rename_fails() {
    use std::os::unix::fs::MetadataExt;

    let directory = test_directory();
    let path = directory.path().join("config.json");
    let initial = fixture_config();
    save_config_to_path(&initial, &path).unwrap();

    assert_eq!(
        fs::metadata(directory.path()).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);

    let mut replacement = fixture_config();
    replacement.inventory_refresh_seconds = 30;
    assert!(save_config_to_path_with_renamer(&replacement, &path, &FailingRenamer).is_err());

    let retained: AppConfig = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(retained.inventory_refresh_seconds, 15);
}

#[cfg(unix)]
#[test]
fn loading_rejects_a_preexisting_non_private_configuration_directory() {
    use std::os::unix::fs::PermissionsExt;

    let directory = test_directory();
    let path = directory.path().join("config.json");
    save_config_to_path(&fixture_config(), &path).unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();

    assert!(load_config_from_path(&path).is_err());
}

#[cfg(unix)]
#[test]
fn loading_rejects_a_preexisting_non_private_configuration_file() {
    use std::os::unix::fs::PermissionsExt;

    let directory = test_directory();
    let path = directory.path().join("config.json");
    save_config_to_path(&fixture_config(), &path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    assert!(load_config_from_path(&path).is_err());
}

#[cfg(unix)]
#[test]
fn saving_rejects_a_preexisting_non_private_configuration_directory() {
    use std::os::unix::fs::PermissionsExt;

    let directory = test_directory();
    let path = directory.path().join("config.json");
    save_config_to_path(&fixture_config(), &path).unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();

    assert!(save_config_to_path(&fixture_config(), &path).is_err());
}

#[cfg(unix)]
#[test]
fn saving_rejects_a_preexisting_non_private_configuration_file() {
    use std::os::unix::fs::PermissionsExt;

    let directory = test_directory();
    let path = directory.path().join("config.json");
    save_config_to_path(&fixture_config(), &path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    assert!(save_config_to_path(&fixture_config(), &path).is_err());
}
