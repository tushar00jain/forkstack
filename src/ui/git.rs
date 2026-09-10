use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

use git2::{Oid, Repository, Sort};

use crate::ui::model::{Commit, Graph, MovePlan};

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
    result
        .status
        .success()
        .then(|| String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

pub fn conflict_label(markers: &[(String, String)], files: &[String]) -> Option<String> {
    if markers.is_empty() {
        return None;
    }
    let kinds = markers
        .iter()
        .map(|(kind, _)| kind.as_str())
        .collect::<Vec<_>>()
        .join("/");
    if files.is_empty() {
        Some(format!("incoming, conflict ({kinds})"))
    } else {
        Some(format!(
            "incoming, conflict ({kinds}: {})",
            files.join(", ")
        ))
    }
}

pub fn load_graph(repo: &Path) -> Result<Graph, String> {
    load_graph_for(repo, "origin", "main")
}

fn add_matching_refs(
    repository: &Repository,
    glob: &str,
    roots: &mut BTreeSet<Oid>,
    refs: &mut Vec<(Oid, String)>,
) -> Result<(), String> {
    let iter = repository
        .references_glob(glob)
        .map_err(|error| error.message().to_owned())?;
    for reference in iter {
        let reference = reference.map_err(|error| error.message().to_owned())?;
        let Some(name) = reference.name() else {
            continue;
        };
        let Ok(commit) = reference.peel_to_commit() else {
            continue;
        };
        roots.insert(commit.id());
        refs.push((commit.id(), name.to_owned()));
    }
    Ok(())
}

fn bounded_ref_globs(remote: &str, base: &str) -> [String; 6] {
    [
        "refs/heads/fs-head/*".into(),
        "refs/heads/fs-base/*".into(),
        format!("refs/heads/{base}"),
        format!("refs/remotes/{remote}/fs-head/*"),
        format!("refs/remotes/{remote}/fs-base/*"),
        format!("refs/remotes/{remote}/{base}"),
    ]
}

fn conflict_paths(repository: &Repository) -> Result<Vec<String>, String> {
    let index = repository
        .index()
        .map_err(|error| error.message().to_owned())?;
    let conflicts = index
        .conflicts()
        .map_err(|error| error.message().to_owned())?;
    let mut paths = BTreeSet::new();
    for conflict in conflicts {
        let conflict = conflict.map_err(|error| error.message().to_owned())?;
        for entry in [conflict.ancestor, conflict.our, conflict.their]
            .into_iter()
            .flatten()
        {
            paths.insert(String::from_utf8_lossy(&entry.path).into_owned());
        }
    }
    Ok(paths.into_iter().collect())
}

pub fn load_graph_for(repo: &Path, remote: &str, base: &str) -> Result<Graph, String> {
    let repository = Repository::discover(repo).map_err(|error| error.message().to_owned())?;
    let head_ref = repository
        .head()
        .map_err(|error| error.message().to_owned())?;
    let head_commit = head_ref
        .peel_to_commit()
        .map_err(|error| error.message().to_owned())?;
    let head_oid = head_commit.id();
    let head = head_oid.to_string();
    let branch = head_ref
        .name()
        .and_then(|name| name.strip_prefix("refs/heads/"))
        .map(str::to_owned);

    let marker_names = [
        ("rebase", "REBASE_HEAD"),
        ("merge", "MERGE_HEAD"),
        ("cherry-pick", "CHERRY_PICK_HEAD"),
    ];
    let markers: Vec<(String, String)> = marker_names
        .iter()
        .filter_map(|(kind, name)| {
            fs::read_to_string(repository.path().join(name))
                .ok()
                .and_then(|contents| contents.split_whitespace().next().map(str::to_owned))
                .map(|id| ((*kind).into(), id))
        })
        .collect();
    let files = conflict_paths(&repository).unwrap_or_default();

    let mut roots = BTreeSet::from([head_oid]);
    let mut refs = Vec::new();
    if let Some(name) = head_ref.name() {
        refs.push((head_oid, name.to_owned()));
    }
    for glob in bounded_ref_globs(remote, base) {
        add_matching_refs(&repository, &glob, &mut roots, &mut refs)?;
    }
    for (_, id) in &markers {
        if let Ok(oid) = Oid::from_str(id) {
            roots.insert(oid);
        }
    }

    let mut walk = repository
        .revwalk()
        .map_err(|error| error.message().to_owned())?;
    walk.set_sorting(Sort::TOPOLOGICAL)
        .map_err(|error| error.message().to_owned())?;
    for root in &roots {
        walk.push(*root)
            .map_err(|error| error.message().to_owned())?;
    }
    let mut commits = HashMap::new();
    let mut order = Vec::new();
    for oid in walk {
        let oid = oid.map_err(|error| error.message().to_owned())?;
        let git_commit = repository
            .find_commit(oid)
            .map_err(|error| error.message().to_owned())?;
        let id = oid.to_string();
        order.push(id.clone());
        commits.insert(
            id.clone(),
            Commit {
                id,
                parents: git_commit
                    .parent_ids()
                    .map(|parent| parent.to_string())
                    .collect(),
                subject: git_commit.summary().unwrap_or_default().to_owned(),
                ..Commit::default()
            },
        );
    }

    for (oid, name) in refs {
        let id = oid.to_string();
        let Some(commit) = commits.get_mut(&id) else {
            continue;
        };
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
    // Tags decorate visible commits but are deliberately not graph roots.
    if let Ok(tags) = repository.references_glob("refs/tags/*") {
        for reference in tags.flatten() {
            let (Some(name), Ok(commit)) = (reference.name(), reference.peel_to_commit()) else {
                continue;
            };
            if let Some(visible) = commits.get_mut(&commit.id().to_string()) {
                visible
                    .tags
                    .push(name.trim_start_matches("refs/tags/").to_owned());
            }
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
        commit.local_refs.dedup();
        commit.remote_refs.sort();
        commit.remote_refs.dedup();
        commit.tags.sort();
        commit.tags.dedup();
    }
    Ok(Graph {
        commits,
        order,
        head,
        branch,
    })
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
    let refs = run(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "--points-at",
            revision,
            "refs/heads",
        ],
    )?;
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
            let id = line
                .split_whitespace()
                .nth(1)
                .ok_or("invalid rebase todo")?
                .to_owned();
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
        let matches: Vec<_> = blocks
            .keys()
            .filter(|id| wanted.starts_with(id.as_str()))
            .cloned()
            .collect();
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

pub fn run_todo_editor(prepared_path: &Path, todo_path: &Path) -> Result<(), String> {
    let prepared = fs::read(prepared_path).map_err(|error| error.to_string())?;
    fs::write(todo_path, prepared).map_err(|error| error.to_string())
}

fn explicit_rebase_todo(plan: &MovePlan) -> String {
    let mut todo = String::new();
    for commit in &plan.commits {
        todo.push_str(&format!("pick {commit}\n"));
        for (_, branch) in plan
            .ref_updates
            .iter()
            .filter(|(id, branch)| id == commit && branch != &plan.checkout_branch)
        {
            todo.push_str(&format!("update-ref refs/heads/{branch}\n"));
        }
    }
    todo
}

fn run_explicit_rebase(repo: &Path, plan: &MovePlan) -> Result<Output, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let todo_path = env::temp_dir().join(format!("forkstack-{}-{stamp}.todo", std::process::id()));
    fs::write(&todo_path, explicit_rebase_todo(plan)).map_err(|error| error.to_string())?;
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let editor = format!(
        "{} todo-editor {}",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(&todo_path.to_string_lossy())
    );
    let upstream = if plan.commits.len() == plan.carried_count {
        &plan.source_base
    } else {
        &plan.destination
    };
    let result = Command::new("git")
        .current_dir(repo)
        .args([
            "-c",
            "rebase.abbreviateCommands=false",
            "rebase",
            "--interactive",
            "--update-refs",
            "--onto",
            &plan.destination,
            upstream,
            &plan.checkout_branch,
        ])
        .env("GIT_SEQUENCE_EDITOR", editor)
        .output()
        .map_err(|error| format!("could not run git: {error}"));
    let _ = fs::remove_file(todo_path);
    result
}

pub fn apply_move(repo: &Path, plan: &MovePlan) -> Result<(), String> {
    assert_clean(repo)?;
    if plan.detach_for_rewrite && !plan.include_descendants {
        run(repo, &["switch", "--detach", &plan.tip_commit])?;
    }
    let result = if plan.include_descendants {
        run_explicit_rebase(repo, plan)?
    } else {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let order_path =
            env::temp_dir().join(format!("forkstack-{}-{stamp}.todo", std::process::id()));
        fs::write(&order_path, format!("{}\n", plan.commits.join("\n")))
            .map_err(|error| error.to_string())?;
        let executable = env::current_exe().map_err(|error| error.to_string())?;
        let editor = format!(
            "{} sequence-editor {}",
            shell_quote(&executable.to_string_lossy()),
            shell_quote(&order_path.to_string_lossy())
        );
        let mut command = Command::new("git");
        command.current_dir(repo).args([
            "-c",
            "rebase.abbreviateCommands=false",
            "rebase",
            "--interactive",
            "--update-refs",
            &plan.base,
        ]);
        if !plan.detach_for_rewrite {
            command.arg(&plan.tip);
        }
        let result = command
            .env("GIT_SEQUENCE_EDITOR", editor)
            .output()
            .map_err(|error| format!("could not run git: {error}"))?;
        let _ = fs::remove_file(order_path);
        result
    };
    if result.status.success() {
        if plan.detach_for_rewrite && !plan.include_descendants {
            run(repo, &["switch", &plan.checkout_branch])?;
        }
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
    PublishPreview(crate::core::submit::SubmitOptions),
    PublishExecute(crate::core::submit::SubmitPlan),
    Stop,
}

#[derive(Debug)]
pub struct Response {
    pub graph: Result<Graph, String>,
    pub preview: Option<Graph>,
    pub publish_plan: Option<crate::core::submit::SubmitPlan>,
    pub operation_error: Option<String>,
}

pub fn worker(
    options: crate::core::submit::SubmitOptions,
    requests: Receiver<Request>,
    responses: Sender<Response>,
) {
    let repo = options.repo.clone();
    while let Ok(request) = requests.recv() {
        let mut preview = None;
        let mut publish_plan = None;
        let mut loaded_graph = None;
        let operation = match request {
            Request::Load => None,
            Request::Checkout(id) => checkout(&repo, &id).err(),
            Request::Apply(plan) => apply_move(&repo, &plan).err(),
            Request::PublishPreview(publish_options) => {
                let remote = publish_options.remote.clone();
                let base = publish_options.base.clone();
                match crate::core::submit::plan(publish_options) {
                    Ok(plan) => match load_graph_for(&repo, &remote, &base) {
                        Ok(graph) => match graph.publish_preview(&plan) {
                            Ok(publish_preview) => {
                                loaded_graph = Some(Ok(graph));
                                preview = Some(publish_preview);
                                publish_plan = Some(plan);
                                None
                            }
                            Err(error) => {
                                loaded_graph = Some(Ok(graph));
                                Some(error)
                            }
                        },
                        Err(error) => {
                            loaded_graph = Some(Err(error.clone()));
                            Some(error)
                        }
                    },
                    Err(error) => Some(error),
                }
            }
            Request::PublishExecute(plan) => crate::core::submit::execute_checked(&plan).err(),
            Request::Stop => break,
        };
        let graph =
            loaded_graph.unwrap_or_else(|| load_graph_for(&repo, &options.remote, &options.base));
        if responses
            .send(Response {
                graph,
                preview,
                publish_plan,
                operation_error: operation,
            })
            .is_err()
        {
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
    fn graph_roots_are_limited_to_forkstack_and_configured_base_refs() {
        assert_eq!(
            bounded_ref_globs("upstream", "trunk"),
            [
                "refs/heads/fs-head/*",
                "refs/heads/fs-base/*",
                "refs/heads/trunk",
                "refs/remotes/upstream/fs-head/*",
                "refs/remotes/upstream/fs-base/*",
                "refs/remotes/upstream/trunk",
            ]
        );
    }

    #[test]
    fn conflict_detection_labels_kind_and_files() {
        let markers = vec![("rebase".into(), "abc".into())];
        assert_eq!(
            conflict_label(&markers, &["src/lib.rs".into()]).unwrap(),
            "incoming, conflict (rebase: src/lib.rs)"
        );
        assert_eq!(conflict_label(&[], &[]), None);
    }

    #[test]
    fn explicit_todo_replays_all_commits_and_updates_non_tip_refs() {
        let plan = MovePlan {
            selected: "beta2".into(),
            destination: "alpha2".into(),
            include_descendants: true,
            base: "alpha2".into(),
            source_base: "beta1".into(),
            carried_count: 2,
            tip: "fs-head/gamma/3".into(),
            tip_commit: "gamma3".into(),
            detach_for_rewrite: true,
            checkout_branch: "fs-head/gamma/3".into(),
            ref_updates: vec![
                ("beta2".into(), "fs-head/beta/2".into()),
                ("beta3".into(), "fs-head/beta/3".into()),
                ("alpha3".into(), "fs-head/alpha/3".into()),
                ("gamma3".into(), "fs-head/gamma/3".into()),
            ],
            commits: vec![
                "beta2".into(),
                "beta3".into(),
                "alpha3".into(),
                "gamma3".into(),
            ],
        };

        assert_eq!(
            explicit_rebase_todo(&plan),
            "pick beta2\n\
             update-ref refs/heads/fs-head/beta/2\n\
             pick beta3\n\
             update-ref refs/heads/fs-head/beta/3\n\
             pick alpha3\n\
             update-ref refs/heads/fs-head/alpha/3\n\
             pick gamma3\n"
        );
    }
}
