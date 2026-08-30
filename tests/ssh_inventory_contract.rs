use std::{fs, path::Path};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use rustedoutclient::{
    cache::InventoryCache,
    model::{NodeName, VmId},
    runtime::RuntimeDir,
    ssh::{InventorySelectionError, InventorySnapshot, VmInventoryItem, VmStatus},
};
use tempfile::{tempdir_in, TempDir};

fn private_tempdir() -> TempDir {
    let temporary_root = std::env::temp_dir().canonicalize().unwrap();
    let directory = tempdir_in(temporary_root).unwrap();
    #[cfg(unix)]
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn vm(vmid: u32, name: &str, status: VmStatus, template: bool) -> VmInventoryItem {
    VmInventoryItem {
        vmid: VmId::new(vmid).unwrap(),
        name: name.to_owned(),
        node: NodeName::parse("pve2").unwrap(),
        status,
        template,
    }
}

fn filenames(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .file_name()
                .into_string()
                .expect("test paths are UTF-8")
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[cfg(unix)]
#[test]
fn runtime_directory_is_short_private_and_raii_owned() {
    let path = {
        let runtime = RuntimeDir::create().unwrap();
        assert!(runtime.path().to_string_lossy().starts_with("/tmp/roc-"));
        assert_eq!(runtime.control_socket(), runtime.path().join("c"));
        assert_eq!(fs::metadata(runtime.path()).unwrap().mode() & 0o777, 0o700);
        runtime.path().to_owned()
    };

    assert!(!path.exists());
}

#[test]
fn snapshot_sorts_filters_templates_and_preserves_stopped_state() {
    let snapshot = InventorySnapshot::new(
        1_777_777_777_000,
        false,
        vec![
            vm(301, "template-base", VmStatus::Stopped, true),
            vm(205, "LabZ1-APP01", VmStatus::Stopped, false),
            vm(107, "LABZ1-CM01", VmStatus::Running, false),
        ],
    );

    assert_eq!(
        snapshot
            .vms
            .iter()
            .map(|item| item.vmid.get())
            .collect::<Vec<_>>(),
        vec![107, 205]
    );
    assert_eq!(snapshot.vms[1].status, VmStatus::Stopped);
    assert!(snapshot.vms.iter().all(|item| !item.template));
}

#[test]
fn selector_is_exact_by_vmid_or_ascii_case_insensitive_name() {
    let snapshot = InventorySnapshot::new(
        1,
        false,
        vec![
            vm(107, "LABZ1-CM01", VmStatus::Running, false),
            vm(205, "labz1-cm01-copy", VmStatus::Stopped, false),
        ],
    );

    assert_eq!(snapshot.select("107").unwrap().name, "LABZ1-CM01");
    assert_eq!(snapshot.select("labz1-cm01").unwrap().vmid.get(), 107);
    assert_eq!(snapshot.select("LABZ1-CM01-COPY").unwrap().vmid.get(), 205);
    assert_eq!(
        snapshot.select("LABZ1-CM").unwrap_err(),
        InventorySelectionError::NotFound
    );
    assert_eq!(
        snapshot.select("0107").unwrap_err(),
        InventorySelectionError::NotFound
    );
}

#[test]
fn duplicate_case_insensitive_names_are_ambiguous() {
    let snapshot = InventorySnapshot::new(
        1,
        false,
        vec![
            vm(107, "LABZ1-CM01", VmStatus::Running, false),
            vm(205, "labz1-cm01", VmStatus::Stopped, false),
        ],
    );

    assert_eq!(
        snapshot.select("LabZ1-Cm01").unwrap_err(),
        InventorySelectionError::Ambiguous
    );
}

#[cfg(unix)]
#[test]
fn cache_is_atomic_private_stale_on_load_and_contains_only_inventory() {
    let directory = private_tempdir();
    let path = directory.path().join("inventory-v1.json");
    let cache = InventoryCache::new(path.clone());
    let first = InventorySnapshot::new(
        1_777_777_777_000,
        false,
        vec![vm(107, "LABZ1-CM01", VmStatus::Running, false)],
    );

    cache.save(&first).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    assert_eq!(filenames(directory.path()), vec!["inventory-v1.json"]);

    let raw = fs::read_to_string(&path).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["observed_at_unix_ms"], 1_777_777_777_000_u64);
    assert!(json.get("ssh_target").is_none());
    assert!(!raw.contains("example.invalid"));
    assert_eq!(
        json.as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["observed_at_unix_ms", "stale", "vms"]
    );

    let loaded = cache.load().unwrap();
    assert!(loaded.stale);
    assert_eq!(loaded.observed_at_unix_ms, first.observed_at_unix_ms);
    assert_eq!(loaded.vms, first.vms);

    let replacement = InventorySnapshot::new(
        1_777_777_888_000,
        false,
        vec![vm(205, "LABZ1-APP01", VmStatus::Stopped, false)],
    );
    cache.save(&replacement).unwrap();
    assert_eq!(filenames(directory.path()), vec!["inventory-v1.json"]);
    let loaded = cache.load().unwrap();
    assert_eq!(loaded.observed_at_unix_ms, 1_777_777_888_000);
    assert_eq!(loaded.vms[0].vmid.get(), 205);
}

#[cfg(unix)]
#[test]
fn cache_rejects_non_private_existing_storage() {
    let directory = private_tempdir();
    let path = directory.path().join("inventory-v1.json");
    let cache = InventoryCache::new(path.clone());
    let snapshot = InventorySnapshot::new(
        1,
        false,
        vec![vm(107, "LABZ1-CM01", VmStatus::Running, false)],
    );

    cache.save(&snapshot).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(cache.load().is_err());
    assert!(cache.save(&snapshot).is_err());

    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(cache.load().is_err());
    assert!(cache.save(&snapshot).is_err());
}

#[cfg(unix)]
#[test]
fn cache_rejects_symlink_parent_and_destination_without_touching_targets() {
    use std::os::unix::fs::symlink;

    let root = private_tempdir();
    let real_directory = root.path().join("real-cache");
    fs::create_dir(&real_directory).unwrap();
    fs::set_permissions(&real_directory, fs::Permissions::from_mode(0o700)).unwrap();
    let linked_directory = root.path().join("linked-cache");
    symlink(&real_directory, &linked_directory).unwrap();
    let snapshot = InventorySnapshot::new(
        1,
        false,
        vec![vm(107, "LABZ1-CM01", VmStatus::Running, false)],
    );

    let parent_link_cache = InventoryCache::new(linked_directory.join("inventory-v1.json"));
    assert!(parent_link_cache.save(&snapshot).is_err());
    assert!(!real_directory.join("inventory-v1.json").exists());

    let outside = root.path().join("outside.json");
    fs::write(&outside, b"outside must remain unchanged\n").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    let destination = real_directory.join("inventory-v1.json");
    symlink(&outside, &destination).unwrap();
    let destination_link_cache = InventoryCache::new(destination);

    assert!(destination_link_cache.load().is_err());
    assert!(destination_link_cache.save(&snapshot).is_err());
    assert_eq!(
        fs::read(&outside).unwrap(),
        b"outside must remain unchanged\n"
    );
}

#[cfg(unix)]
#[test]
fn cache_rejects_a_symlinked_ancestor_without_creating_beneath_its_target() {
    use std::os::unix::fs::symlink;

    let root = private_tempdir();
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
    let linked_ancestor = root.path().join("linked-ancestor");
    symlink(&outside, &linked_ancestor).unwrap();
    let cache = InventoryCache::new(
        linked_ancestor
            .join("private-cache")
            .join("inventory-v1.json"),
    );
    let snapshot = InventorySnapshot::new(
        1,
        false,
        vec![vm(107, "LABZ1-CM01", VmStatus::Running, false)],
    );

    assert!(cache.save(&snapshot).is_err());
    assert!(!outside.join("private-cache").exists());
}

#[cfg(unix)]
#[test]
fn cache_creates_an_absent_leaf_directory_with_mode_0700() {
    let root = private_tempdir();
    let stable_parent = root.path().join("stable-parent");
    fs::create_dir(&stable_parent).unwrap();
    fs::set_permissions(&stable_parent, fs::Permissions::from_mode(0o700)).unwrap();
    let leaf = stable_parent.join("private-cache");
    let path = leaf.join("inventory-v1.json");
    let cache = InventoryCache::new(path.clone());
    let snapshot = InventorySnapshot::new(
        1,
        false,
        vec![vm(107, "LABZ1-CM01", VmStatus::Running, false)],
    );

    cache.save(&snapshot).unwrap();

    assert_eq!(fs::metadata(&leaf).unwrap().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn cache_rejects_unknown_fields_duplicate_vmids_and_invalid_names() {
    let directory = private_tempdir();
    let path = directory.path().join("inventory-v1.json");
    let cache = InventoryCache::new(path.clone());
    let snapshot = InventorySnapshot::new(
        1,
        false,
        vec![vm(107, "LABZ1-CM01", VmStatus::Running, false)],
    );
    cache.save(&snapshot).unwrap();
    let valid: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

    let mut cases = Vec::new();
    let mut unknown_top_level = valid.clone();
    unknown_top_level["unexpected"] = serde_json::json!(true);
    cases.push(unknown_top_level);

    let mut unknown_vm_field = valid.clone();
    unknown_vm_field["vms"][0]["unexpected"] = serde_json::json!(true);
    cases.push(unknown_vm_field);

    let mut duplicate = valid.clone();
    let duplicated_item = duplicate["vms"][0].clone();
    duplicate["vms"]
        .as_array_mut()
        .unwrap()
        .push(duplicated_item);
    cases.push(duplicate);

    let mut empty_name = valid.clone();
    empty_name["vms"][0]["name"] = serde_json::json!("");
    cases.push(empty_name);

    let mut control_name = valid;
    control_name["vms"][0]["name"] = serde_json::json!("bad\nname");
    cases.push(control_name);

    for invalid in cases {
        fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(cache.load().is_err(), "accepted invalid cache: {invalid}");
    }
}

#[cfg(unix)]
#[test]
fn cache_rejects_malformed_status_node_and_vmid_fields() {
    let directory = private_tempdir();
    let path = directory.path().join("inventory-v1.json");
    let cache = InventoryCache::new(path.clone());
    let snapshot = InventorySnapshot::new(
        1,
        false,
        vec![vm(107, "LABZ1-CM01", VmStatus::Running, false)],
    );
    cache.save(&snapshot).unwrap();
    let valid: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

    for (field, invalid_value) in [
        ("status", serde_json::json!("paused")),
        ("node", serde_json::json!("not/a/node")),
        ("vmid", serde_json::json!(99)),
    ] {
        let mut invalid = valid.clone();
        invalid["vms"][0][field] = invalid_value;
        fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(
            cache.load().is_err(),
            "accepted malformed cached {field}: {invalid}"
        );
    }
}
