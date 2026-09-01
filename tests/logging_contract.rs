use std::{fs, io::Write, path::Path};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use rustedoutclient::logging::{log_path, PrivateRollingLog};

#[test]
fn log_path_stays_inside_the_private_application_directory() {
    let home = Path::new("/synthetic/home");
    assert_eq!(
        log_path(home),
        home.join("Library")
            .join("Application Support")
            .join("RustedOutClient")
            .join("diagnostics.log")
    );
}

#[cfg(unix)]
#[test]
fn rolling_log_creates_private_storage_and_rotates_before_exceeding_the_bound() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("private").join("diagnostics.log");
    let backup = root.path().join("private").join("diagnostics.log.1");
    let mut log = PrivateRollingLog::open(&path, 24).unwrap();

    log.write_all(b"first-safe-record\n").unwrap();
    log.flush().unwrap();
    assert_eq!(
        fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);

    log.write_all(b"second-safe-record\n").unwrap();
    log.flush().unwrap();
    assert_eq!(fs::read(&backup).unwrap(), b"first-safe-record\n");
    assert_eq!(fs::read(&path).unwrap(), b"second-safe-record\n");
    assert!(fs::metadata(&path).unwrap().len() <= 24);
    assert_eq!(fs::metadata(&backup).unwrap().mode() & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn rolling_log_rejects_preexisting_public_or_non_regular_storage() {
    let root = tempfile::tempdir().unwrap();
    let public_directory = root.path().join("public");
    fs::create_dir(&public_directory).unwrap();
    fs::set_permissions(&public_directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(PrivateRollingLog::open(&public_directory.join("diagnostics.log"), 128).is_err());

    let private_directory = root.path().join("private");
    fs::create_dir(&private_directory).unwrap();
    fs::set_permissions(&private_directory, fs::Permissions::from_mode(0o700)).unwrap();
    let target = private_directory.join("target");
    fs::write(&target, b"target").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    let linked = private_directory.join("diagnostics.log");
    std::os::unix::fs::symlink(&target, &linked).unwrap();
    assert!(PrivateRollingLog::open(&linked, 128).is_err());
}
