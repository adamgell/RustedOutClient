use std::{env, path::PathBuf, process};

use rustedoutclient_fuzz::{load_manifest, verify_manifest};

fn main() {
    let mut manifest = None;
    let mut root = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--manifest" => {
                manifest = Some(required_value("--manifest", args.next()));
            }
            "--root" => {
                root = Some(required_value("--root", args.next()));
            }
            other => {
                eprintln!("usage: verify_seeds [--manifest PATH] [--root DIR]");
                eprintln!("unknown argument {other}");
                process::exit(2);
            }
        }
    }

    let manifest_path = manifest
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus-manifest.json"));
    let root_path = root.unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let loaded = match load_manifest(&manifest_path) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("{error}");
            process::exit(1);
        }
    };
    match verify_manifest(&loaded, &root_path) {
        Ok(()) => {}
        Err(failures) => {
            for failure in failures {
                eprintln!("{failure}");
            }
            process::exit(1);
        }
    }
}

fn required_value(flag: &str, value: Option<String>) -> PathBuf {
    match value {
        Some(value) => PathBuf::from(value),
        None => {
            eprintln!("usage: verify_seeds [--manifest PATH] [--root DIR]");
            eprintln!("{flag} requires a path");
            process::exit(2);
        }
    }
}
