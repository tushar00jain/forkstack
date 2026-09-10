mod app;
mod git;
mod model;
mod render;

use std::env;
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!("usage: forkstack-ui [--repo PATH]\n       forkstack-ui --help");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--sequence-editor") {
        if args.len() != 3 {
            usage();
        }
        if let Err(error) =
            git::run_sequence_editor(&PathBuf::from(&args[1]), &PathBuf::from(&args[2]))
        {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("forkstack-ui [--repo PATH]\n\nA local-only Git stack graph and history editor.");
        return;
    }
    let mut repo = PathBuf::from(".");
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--repo" if index + 1 < args.len() => {
                repo = PathBuf::from(&args[index + 1]);
                index += 2;
            }
            _ => usage(),
        }
    }
    if let Err(error) = app::App::new(repo).run() {
        eprintln!("forkstack-ui: {error}");
        std::process::exit(1);
    }
}
