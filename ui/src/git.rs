use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::{Commit, Graph, MovePlan};

fn output(repo: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|error| format!("could not run git: {error}"))
}

fn run(repo: &Path, args: &[&str]) -> Result<String, String> {
    let result = output(repo, args)?;
    if result.status.success() {
        Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&result.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&result.stdout).trim().to_owned();
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
}

fn verify(repo: &Path, revision: &str) -> Option<String> {
    let result = output(repo, &["rev-parse", "--verify", "-q", revision]).ok()?;
    result.status.success().then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

pub fn conflict_label(markers: &[(String, String)], files: &[String]) -> Option<String> {
    if markers.is_empty() {
        return None;
    }
    let kinds = markers.iter().map(|(kind, _)| kind.as_str()).collect::<Vec<_>>().join("/");
    if files.is_empty() {
        Some(format!("incoming, conflict ({kinds})"))
    } else {
        Some(format!("incoming, conflict ({kinds}: {})", files.join(", ")))
    }
}

fn parse_log(log: &str) -> Result<(HashMap<String, Commit>, Vec<String>), String> {
    let mut commits = HashMap::new();
    let mut order = Vec::new();
    for record in log.split('\x1e') {
        let record = record.trim_matches(|character| character == '\n' || character == '\r');
        if record.is_empty() {
            continue;
        }
        let fields: Vec<_> = record.splitn(3, '\x1f').collect();
        if fields.len() != 3 {
            return Err("git log returned an unexpected record".into());
        }
        let id = fields[0].to_owned();
        order.push(id.clone());
        commits.insert(id.clone(), Commit {
            id,
            parents: fields[1].split_whitespace().map(str::to_owned).collect(),
            subject: fields[2].to_owned(),
            ..Commit::default()
        });
    }
    Ok((commits, order))
}

pub fn load_graph(repo: &Path) -> Result<Graph, String> {
    run(repo, &["rev-parse", "--git-dir"])?;
    let head = run(repo, &["rev-parse", "HEAD"])?;
    let branch = output(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .filter(|result| result.status.success())
        .map(|result| String::from_utf8_lossy(&result.stdout).trim().to_owned());

    let marker_names = [
        ("rebase", "REBASE_HEAD"),
        ("merge", "MERGE_HEAD"),
        ("cherry-pick", "CHERRY_PICK_HEAD"),
    ];
    let markers: Vec<(String, String)> = marker_names
        .iter()
        .filter_map(|(kind, name)| verify(repo, name).map(|id| ((*kind).into(), id)))
        .collect();
    let files = run(repo, &["diff", "--name-only", "--diff-filter=U"])
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();

    let mut command = Command::new("git");
    command.current_dir(repo).args([
        "log",
        "--branches",
        "--remotes",
        "HEAD",
        "--topo-order",
        "--format=%H%x1f%P%x1f%s%x1e",
    ]);
    for (_, id) in &markers {
        command.arg(id);
    }
    let result = command.output().map_err(|error| format!("could not run git: {error}"))?;
    if !result.status.success() {
        return Err(String::from_utf8_lossy(&result.stderr).trim().to_owned());
    }
    let log = String::from_utf8_lossy(&result.stdout);
    let (mut commits, order) = parse_log(&log)?;

    let refs = run(
        repo,
        &["for-each-ref", "--format=%(objectname)%00%(refname)", "refs/heads", "refs/remotes"],
    )?;
    for line in refs.lines() {
        let Some((id, name)) = line.split_once('\0') else { continue };
        let Some(commit) = commits.get_mut(id) else { continue };
        if let Some(name) = name.strip_prefix("refs/heads/") {
            commit.local_refs.push(name.into());
        } else if let Some(name) = name.strip_prefix("refs/remotes/") {
            // Symbolic remote HEAD refs point at the same commit and are useful
            // only as clutter in this view.
            if !name.ends_with("/HEAD") {
                commit.remote_refs.push(name.into());
            }
        } else if let Some(name) = name.strip_prefix("refs/tags/") {
            commit.tags.push(name.into());
        }
    }
    if let Some(commit) = commits.get_mut(&head) {
        commit.is_head = true;
        if !markers.is_empty() {
            commit.conflict = Some("local (conflict in progress)".into());
        }
    }
    let incoming = conflict_label(&markers, &files);
    for (_, id) in &markers {
        if let Some(commit) = commits.get_mut(id) {
            commit.conflict = incoming.clone();
        }
    }
    for commit in commits.values_mut() {
        commit.local_refs.sort();
        commit.remote_refs.sort();
        commit.tags.sort();
    }
    Ok(Graph { commits, order, head, branch })
}

fn assert_clean(repo: &Path) -> Result<(), String> {
    if !run(repo, &["status", "--porcelain"])?.is_empty() {
        return Err("the working tree must be clean".into());
    }
    for state in ["REBASE_HEAD", "MERGE_HEAD", "CHERRY_PICK_HEAD"] {
        if verify(repo, state).is_some() {
            return Err("finish the current Git operation first".into());
        }
    }
    let git_dir = PathBuf::from(run(repo, &["rev-parse", "--absolute-git-dir"])?);
    if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
        return Err("finish the current Git operation first".into());
    }
    Ok(())
}

pub fn checkout(repo: &Path, revision: &str) -> Result<(), String> {
    assert_clean(repo)?;
    let refs = run(repo, &["for-each-ref", "--format=%(refname:short)", "--points-at", revision, "refs/heads"])?;
    let branches: Vec<_> = refs.lines().filter(|line| !line.is_empty()).collect();
    if branches.len() == 1 {
        run(repo, &["switch", branches[0]])?;
    } else {
        run(repo, &["switch", "--detach", revision])?;
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn reorder_todo(original: &str, order: &[String]) -> Result<String, String> {
    let mut blocks: HashMap<String, Vec<String>> = HashMap::new();
    let mut comments = Vec::new();
    let mut current: Option<String> = None;
    for line in original.split_inclusive('\n') {
        if line.starts_with("pick ") {
            let id = line.split_whitespace().nth(1).ok_or("invalid rebase todo")?.to_owned();
            blocks.insert(id.clone(), vec![line.into()]);
            current = Some(id);
        } else if !line.starts_with('#') {
            if let Some(id) = current.as_ref() {
                blocks.get_mut(id).unwrap().push(line.into());
            } else {
                comments.push(line.to_owned());
            }
        } else {
            current = None;
            comments.push(line.to_owned());
        }
    }
    let mut arranged = String::new();
    for wanted in order {
        let matches: Vec<_> = blocks.keys().filter(|id| wanted.starts_with(id.as_str())).cloned().collect();
        if matches.len() != 1 {
            return Err(format!("could not identify {wanted} in rebase todo"));
        }
        for line in blocks.remove(&matches[0]).unwrap() {
            arranged.push_str(&line);
        }
    }
    for line in comments {
        arranged.push_str(&line);
    }
    Ok(arranged)
}

pub fn run_sequence_editor(order_path: &Path, todo_path: &Path) -> Result<(), String> {
    let order = fs::read_to_string(order_path).map_err(|error| error.to_string())?;
    let original = fs::read_to_string(todo_path).map_err(|error| error.to_string())?;
    let order: Vec<String> = order.lines().map(str::to_owned).collect();
    fs::write(todo_path, reorder_todo(&original, &order)?).map_err(|error| error.to_string())
}

pub fn apply_move(repo: &Path, plan: &MovePlan) -> Result<(), String> {
    assert_clean(repo)?;
    let result = if plan.include_descendants {
        output(repo, &["rebase", "--update-refs", "--onto", &plan.destination, &plan.base, &plan.tip])?
    } else {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let order_path = env::temp_dir().join(format!("forkstack-ui-{}-{stamp}.todo", std::process::id()));
        fs::write(&order_path, format!("{}\n", plan.commits.join("\n"))).map_err(|error| error.to_string())?;
        let executable = env::current_exe().map_err(|error| error.to_string())?;
        let editor = format!("{} --sequence-editor {}", shell_quote(&executable.to_string_lossy()), shell_quote(&order_path.to_string_lossy()));
        let result = Command::new("git")
            .current_dir(repo)
            .args(["-c", "rebase.abbreviateCommands=false", "rebase", "--interactive", "--update-refs", &plan.base, &plan.tip])
            .env("GIT_SEQUENCE_EDITOR", editor)
            .output()
            .map_err(|error| format!("could not run git: {error}"))?;
        let _ = fs::remove_file(order_path);
        result
    };
    if result.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&result.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&result.stdout).trim().to_owned();
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
}

#[derive(Debug)]
pub enum Request {
    Load,
    Checkout(String),
    Apply(MovePlan),
    Stop,
}

#[derive(Debug)]
pub struct Response {
    pub graph: Result<Graph, String>,
    pub operation_error: Option<String>,
}

pub fn worker(repo: PathBuf, requests: Receiver<Request>, responses: Sender<Response>) {
    while let Ok(request) = requests.recv() {
        let operation = match request {
            Request::Load => None,
            Request::Checkout(id) => checkout(&repo, &id).err(),
            Request::Apply(plan) => apply_move(&repo, &plan).err(),
            Request::Stop => break,
        };
        let graph = load_graph(&repo);
        if responses.send(Response { graph, operation_error: operation }).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_reorders_rebase_blocks() {
        let todo = "pick aaaaaaa first\nupdate-ref refs/heads/a\n\npick bbbbbbb second\n# help\n";
        let result = reorder_todo(todo, &["bbbbbbbb".into(), "aaaaaaaa".into()]).unwrap();
        assert!(result.find("pick bbbbbbb").unwrap() < result.find("pick aaaaaaa").unwrap());
        assert!(result.find("refs/heads/a").unwrap() > result.find("pick aaaaaaa").unwrap());
    }

    #[test]
    fn parses_git_log_records_and_ordered_parents() {
        let input = "bbbb\x1faaaa cccc\x1fmerge subject\x1e\naaaa\x1f\x1froot\x1e\n";
        let (commits, order) = parse_log(input).unwrap();
        assert_eq!(order, vec!["bbbb".to_owned(), "aaaa".to_owned()]);
        assert_eq!(commits["bbbb"].parents, vec!["aaaa".to_owned(), "cccc".to_owned()]);
        assert_eq!(commits["aaaa"].subject, "root");
    }

    #[test]
    fn conflict_detection_labels_kind_and_files() {
        let markers = vec![("rebase".into(), "abc".into())];
        assert_eq!(conflict_label(&markers, &["src/lib.rs".into()]).unwrap(), "incoming, conflict (rebase: src/lib.rs)");
        assert_eq!(conflict_label(&[], &[]), None);
    }
}
