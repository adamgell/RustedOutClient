use std::{env, path::Path, process};

use rustedoutclient_fuzz::write_candidates;

fn main() {
    let mut args = env::args().skip(1);
    let Some(output) = args.next() else {
        eprintln!("usage: build_seeds <absolute-output-dir>");
        process::exit(2);
    };
    if args.next().is_some() {
        eprintln!("usage: build_seeds <absolute-output-dir>");
        process::exit(2);
    }
    match write_candidates(Path::new(&output)) {
        Ok(manifest) => {
            println!("wrote {} candidates", manifest.seeds.len());
        }
        Err(error) => {
            eprintln!("{error}");
            process::exit(1);
        }
    }
}
