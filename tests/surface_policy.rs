use std::{fs, path::Path};

#[test]
fn product_surface_excludes_removed_features_and_password_cli() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();

    for forbidden in ["russh-sftp", "rfd =", "eax =", "aes =", "rsa ="] {
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
