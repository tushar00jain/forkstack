use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const REFRESH_STATUS: i32 = b'R' as i32;
const LESSKEY: &str = "#command\nR quit R\n";
const LESS_ARGS: &[&str] = &[
    "-R",
    "-S",
    "-+F",
    "-X",
    "-Ps?e(END)  .[R] refresh   [q] quit",
];
const CLEAR: &str = "\x1b[H\x1b[2J";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogSpec {
    pub remotes: Vec<String>,
    pub tags: bool,
    pub max_count: Option<i64>,
    pub git_args: Vec<String>,
}

pub fn normalize_remotes(specs: &[String], no_remotes: bool) -> Vec<String> {
    if no_remotes {
        return Vec::new();
    }
    let remotes: Vec<_> = specs
        .iter()
        .flat_map(|spec| spec.split(','))
        .filter(|remote| !remote.is_empty())
        .map(str::to_owned)
        .collect();
    if remotes.is_empty() {
        vec!["origin".into()]
    } else {
        remotes
    }
}

pub fn git_log_args(spec: &LogSpec, color: bool) -> Vec<String> {
    let mut args = vec![
        "log".into(),
        "--graph".into(),
        "--oneline".into(),
        "--decorate".into(),
    ];
    if color {
        args.insert(1, "--color=always".into());
    }
    args.extend(["--branches".into(), "HEAD".into()]);
    args.extend(
        spec.remotes
            .iter()
            .map(|remote| format!("--glob=refs/remotes/{remote}/*")),
    );
    let mut decoration_refs = vec!["refs/heads/*".to_owned(), "HEAD".to_owned()];
    decoration_refs.extend(
        spec.remotes
            .iter()
            .map(|remote| format!("refs/remotes/{remote}/*")),
    );
    if spec.tags {
        args.push("--tags".into());
        decoration_refs.push("refs/tags/*".into());
    }
    args.extend(
        decoration_refs
            .into_iter()
            .map(|reference| format!("--decorate-refs={reference}")),
    );
    if let Some(count) = spec.max_count.filter(|count| *count != 0) {
        args.push(format!("-{count}"));
    }
    args.extend(spec.git_args.iter().cloned());
    args
}

pub fn run(repo: &Path, spec: &LogSpec) -> Result<(), String> {
    if !io::stdout().is_terminal() {
        Command::new("git")
            .args(git_log_args(spec, false))
            .current_dir(repo)
            .status()
            .map_err(|error| format!("could not run git: {error}"))?;
        return Ok(());
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let key_path =
        env::temp_dir().join(format!("forkstack-{}-{stamp}.lesskey", std::process::id()));
    fs::write(&key_path, LESSKEY).map_err(|error| error.to_string())?;
    let result = loop {
        match page(repo, spec, &key_path) {
            Ok(true) => {
                print!("{CLEAR}");
                io::stdout().flush().map_err(|error| error.to_string())?;
            }
            Ok(false) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    let _ = fs::remove_file(key_path);
    result
}

fn page(repo: &Path, spec: &LogSpec, key_path: &Path) -> Result<bool, String> {
    let mut walk = Command::new("git")
        .args(git_log_args(spec, true))
        .current_dir(repo)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run git: {error}"))?;
    let input = walk.stdout.take().ok_or("could not read git log output")?;
    let status = Command::new("less")
        .args(LESS_ARGS)
        .env("LESSKEYIN", key_path)
        .stdin(input)
        .status()
        .map_err(|error| format!("could not run less: {error}"));
    let _ = walk.wait();
    Ok(status?.code() == Some(REFRESH_STATUS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_specs_are_repeatable_comma_separated_and_default_origin() {
        assert_eq!(normalize_remotes(&[], false), ["origin"]);
        assert_eq!(
            normalize_remotes(&["one,two".into(), "three".into()], false),
            ["one", "two", "three"]
        );
        assert!(normalize_remotes(&["one".into()], true).is_empty());
    }

    #[test]
    fn constructs_filtered_git_log_command() {
        let args = git_log_args(
            &LogSpec {
                remotes: vec!["origin".into(), "upstream".into()],
                tags: true,
                max_count: Some(20),
                git_args: vec!["--first-parent".into()],
            },
            false,
        );
        assert_eq!(&args[..4], ["log", "--graph", "--oneline", "--decorate"]);
        for expected in [
            "--branches",
            "HEAD",
            "--glob=refs/remotes/origin/*",
            "--glob=refs/remotes/upstream/*",
            "--tags",
            "--decorate-refs=refs/heads/*",
            "--decorate-refs=HEAD",
            "--decorate-refs=refs/remotes/origin/*",
            "--decorate-refs=refs/remotes/upstream/*",
            "--decorate-refs=refs/tags/*",
            "-20",
            "--first-parent",
        ] {
            assert!(args.iter().any(|arg| arg == expected), "missing {expected}");
        }
        assert_eq!(
            git_log_args(
                &LogSpec {
                    remotes: vec![],
                    tags: false,
                    max_count: None,
                    git_args: vec![]
                },
                true
            )[1],
            "--color=always"
        );
    }
}
