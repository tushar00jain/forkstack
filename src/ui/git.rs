use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use git2::{Oid, Repository, RepositoryState, Sort};

use crate::integrations::git::delete_remote_branches;
use crate::integrations::github::{PullRequestLink, discover_pr_links};
use crate::integrations::{CommandRunner, ProcessRunner};
use crate::ui::model::{Commit, DeleteBranchPlan, Graph, MoveMode, MovePlan, ReplayPlan};

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

fn is_graph_root_ref(name: &str, base: &str, remotes: &BTreeSet<&str>) -> bool {
    let is_visible_remote_ref = name
        .strip_prefix("refs/remotes/")
        .and_then(|name| name.split_once('/'))
        .is_some_and(|(remote, branch)| {
            remotes.contains(remote)
                && (branch == base
                    || branch.starts_with("fs-head/")
                    || branch.starts_with("fs-base/"))
        });
    name.starts_with("refs/heads/") || is_visible_remote_ref
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

fn conflict_marker_names(state: RepositoryState) -> &'static [(&'static str, &'static str)] {
    match state {
        RepositoryState::Rebase
        | RepositoryState::RebaseInteractive
        | RepositoryState::RebaseMerge
        | RepositoryState::ApplyMailboxOrRebase => &[("rebase", "REBASE_HEAD")],
        RepositoryState::Merge => &[("merge", "MERGE_HEAD")],
        RepositoryState::CherryPick | RepositoryState::CherryPickSequence => {
            &[("cherry-pick", "CHERRY_PICK_HEAD")]
        }
        _ => &[],
    }
}

pub fn load_graph_for(repo: &Path, remote: &str, base: &str) -> Result<Graph, String> {
    let repository = Repository::discover(repo).map_err(|error| error.message().to_owned())?;
    let head_ref = match repository.head() {
        Ok(head) => head,
        Err(error) if error.code() == git2::ErrorCode::UnbornBranch => {
            return Ok(Graph::default());
        }
        Err(error) => return Err(error.message().to_owned()),
    };
    let head_commit = head_ref
        .peel_to_commit()
        .map_err(|error| error.message().to_owned())?;
    let head_oid = head_commit.id();
    let head = head_oid.to_string();
    let branch = head_ref
        .name()
        .and_then(|name| name.strip_prefix("refs/heads/"))
        .map(str::to_owned);

    // REBASE_HEAD can survive a completed rebase as a historical pseudo-ref.
    // Only operation markers matching libgit2's current repository state imply
    // that a conflict is still in progress.
    let markers: Vec<(String, String)> = conflict_marker_names(repository.state())
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
    let visible_remotes = BTreeSet::from(["origin", "upstream", remote]);
    let mut tags = Vec::new();
    for reference in repository
        .references()
        .map_err(|error| error.message().to_owned())?
    {
        let reference = reference.map_err(|error| error.message().to_owned())?;
        let Some(name) = reference.name() else {
            continue;
        };
        let is_root = is_graph_root_ref(name, base, &visible_remotes);
        let is_tag = name.starts_with("refs/tags/");
        if !is_root && !is_tag {
            continue;
        }
        let Ok(commit) = reference.peel_to_commit() else {
            continue;
        };
        if is_root {
            roots.insert(commit.id());
            refs.push((commit.id(), name.to_owned()));
        } else {
            tags.push((commit.id(), name.to_owned()));
        }
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
    for (oid, name) in tags {
        if let Some(visible) = commits.get_mut(&oid.to_string()) {
            visible
                .tags
                .push(name.trim_start_matches("refs/tags/").to_owned());
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

pub fn checkout(repo: &Path, revision: &str, branch: Option<&str>) -> Result<(), String> {
    if let Some(branch) = branch {
        run(repo, &["switch", branch])?;
    } else {
        run(repo, &["switch", "--detach", revision])?;
    }
    Ok(())
}

/// Force every existing local `fs-head/*` branch to the matching fetched ref
/// for `remote`. Branches without a matching remote ref are left alone.
pub fn reset_local_heads(repo: &Path, remote: &str) -> Result<String, String> {
    let repository = Repository::discover(repo).map_err(|error| error.message().to_owned())?;
    if repository.state() != RepositoryState::Clean {
        return Err("cannot reset local branches while a Git operation is in progress".into());
    }
    if !run(repo, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        return Err("cannot reset local branches with uncommitted tracked changes".into());
    }

    let current = repository
        .head()
        .ok()
        .filter(|head| head.is_branch())
        .and_then(|head| head.name().map(str::to_owned));
    let mut checked_out_elsewhere = BTreeSet::new();
    for name in repository
        .worktrees()
        .map_err(|error| error.message().to_owned())?
        .iter()
        .flatten()
    {
        let worktree = repository
            .find_worktree(name)
            .map_err(|error| error.message().to_owned())?;
        let worktree_repo =
            Repository::open(worktree.path()).map_err(|error| error.message().to_owned())?;
        if let Ok(head) = worktree_repo.head()
            && let Some(name) = head.name()
            && Some(name) != current.as_deref()
        {
            checked_out_elsewhere.insert(name.to_owned());
        }
    }

    let mut updates = Vec::new();
    let mut missing = Vec::new();
    for reference in repository
        .references_glob("refs/heads/fs-head/*")
        .map_err(|error| error.message().to_owned())?
    {
        let reference = reference.map_err(|error| error.message().to_owned())?;
        let Some(local_name) = reference.name() else {
            continue;
        };
        let short_name = local_name.trim_start_matches("refs/heads/");
        let remote_name = format!("refs/remotes/{remote}/{short_name}");
        let Ok(remote_ref) = repository.find_reference(&remote_name) else {
            missing.push(short_name.to_owned());
            continue;
        };
        let old = reference
            .peel_to_commit()
            .map_err(|error| error.message().to_owned())?
            .id();
        let wanted = remote_ref
            .peel_to_commit()
            .map_err(|error| error.message().to_owned())?
            .id();
        if old != wanted {
            if checked_out_elsewhere.contains(local_name) {
                return Err(format!(
                    "cannot reset {short_name:?}: it is checked out in another worktree"
                ));
            }
            updates.push((local_name.to_owned(), old, wanted));
        }
    }

    if !updates.is_empty() {
        let mut transaction = repository
            .transaction()
            .map_err(|error| error.message().to_owned())?;
        for (name, _, _) in &updates {
            transaction
                .lock_ref(name)
                .map_err(|error| error.message().to_owned())?;
        }
        for (name, old, wanted) in &updates {
            let actual = repository
                .find_reference(name)
                .ok()
                .and_then(|reference| reference.peel_to_commit().ok())
                .map(|commit| commit.id());
            if actual != Some(*old) {
                return Err(format!("reference {name} changed while preparing updates"));
            }
            transaction
                .set_target(
                    name,
                    *wanted,
                    None,
                    "forkstack: reset local branch to remote",
                )
                .map_err(|error| error.message().to_owned())?;
        }
        transaction
            .commit()
            .map_err(|error| error.message().to_owned())?;
    }

    if current
        .as_ref()
        .is_some_and(|current| updates.iter().any(|(name, _, _)| name == current))
    {
        run(repo, &["reset", "--hard", "HEAD"])?;
    }

    let updated = updates.len();
    let skipped = missing.len();
    let mut status = match updated {
        0 => format!("local branches already match {remote}"),
        1 => format!("reset 1 local branch to {remote}"),
        _ => format!("reset {updated} local branches to {remote}"),
    };
    if skipped != 0 {
        status.push_str(&format!(
            "; skipped {skipped} without a matching remote branch"
        ));
    }
    Ok(status)
}

pub fn delete_forkstack_branch(repo: &Path, plan: &DeleteBranchPlan) -> Result<String, String> {
    delete_forkstack_branch_with(repo, plan, &ProcessRunner)
}

fn delete_forkstack_branch_with(
    repo: &Path,
    plan: &DeleteBranchPlan,
    runner: &dyn CommandRunner,
) -> Result<String, String> {
    let repository = Repository::discover(repo).map_err(|error| error.message().to_owned())?;
    if repository.state() != RepositoryState::Clean {
        return Err("cannot delete branches while a Git operation is in progress".into());
    }
    if !run(repo, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        return Err("cannot delete branches with uncommitted tracked changes".into());
    }

    let head = repository
        .head()
        .map_err(|error| error.message().to_owned())?;
    if head.shorthand() != Some(&plan.branch) {
        return Err("checked-out branch changed after the deletion preview".into());
    }
    let actual_head = head
        .peel_to_commit()
        .map_err(|error| error.message().to_owned())?
        .id()
        .to_string();
    if actual_head != plan.expected_head {
        return Err("checked-out branch moved after the deletion preview".into());
    }
    let checkout = repository
        .find_branch(&plan.checkout_branch, git2::BranchType::Local)
        .map_err(|_| {
            format!(
                "local base branch {:?} no longer exists",
                plan.checkout_branch
            )
        })?;
    let actual_checkout = checkout
        .get()
        .peel_to_commit()
        .map_err(|error| error.message().to_owned())?
        .id()
        .to_string();
    if actual_checkout != plan.expected_checkout {
        return Err("local base branch moved after the deletion preview".into());
    }
    let configured_remotes: BTreeSet<_> = repository
        .remotes()
        .map_err(|error| error.message().to_owned())?
        .iter()
        .flatten()
        .map(str::to_owned)
        .collect();
    drop(checkout);
    drop(head);
    drop(repository);

    run(repo, &["switch", &plan.checkout_branch])?;

    let mut num_remote_branches = 0;
    for remote in ["origin", "upstream"] {
        let branches: Vec<_> = plan
            .remote_branches
            .iter()
            .filter(|(candidate, _)| candidate == remote)
            .map(|(_, branch)| branch.clone())
            .collect();
        if configured_remotes.contains(remote) {
            num_remote_branches +=
                delete_remote_branches(runner, repo, remote, branches.iter().cloned())?;
        }
        crate::integrations::git::delete_refs(
            repo,
            &branches
                .iter()
                .map(|branch| format!("refs/remotes/{remote}/{branch}"))
                .collect::<Vec<_>>(),
        )?;
    }
    run(repo, &["branch", "-D", &plan.branch])?;

    Ok(format!(
        "deleted {} locally and {num_remote_branches} remote branch{}",
        plan.branch,
        if num_remote_branches == 1 { "" } else { "es" }
    ))
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

fn explicit_rebase_todo(plan: &ReplayPlan) -> String {
    let mut todo = String::new();
    for commit in &plan.commits {
        todo.push_str(&format!("pick {commit}\n"));
        for (_, branch) in plan
            .ref_updates
            .iter()
            .filter(|(id, branch)| id == commit && branch != &plan.branch)
        {
            todo.push_str(&format!("update-ref refs/heads/{branch}\n"));
        }
    }
    todo
}

fn run_explicit_rebase(
    repo: &Path,
    plan: &ReplayPlan,
    executable: &Path,
) -> Result<Output, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let todo_path = env::temp_dir().join(format!("forkstack-{}-{stamp}.todo", std::process::id()));
    fs::write(&todo_path, explicit_rebase_todo(plan)).map_err(|error| error.to_string())?;
    let editor = format!(
        "{} todo-editor {}",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(&todo_path.to_string_lossy())
    );
    let result = Command::new("git")
        .current_dir(repo)
        .args([
            "-c",
            "rebase.abbreviateCommands=false",
            "rebase",
            "--interactive",
            "--update-refs",
            "--onto",
            &plan.onto,
            &plan.upstream,
            &plan.branch,
        ])
        .env("GIT_SEQUENCE_EDITOR", editor)
        .output()
        .map_err(|error| format!("could not run git: {error}"));
    let _ = fs::remove_file(todo_path);
    result
}

pub fn apply_move(repo: &Path, plan: &MovePlan) -> Result<(), String> {
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    apply_move_with_executable(repo, plan, &executable)
}

fn apply_move_with_executable(
    repo: &Path,
    plan: &MovePlan,
    executable: &Path,
) -> Result<(), String> {
    let detached_rewrite =
        plan.mode == MoveMode::Reorder && plan.detach_for_rewrite && !plan.include_descendants;
    let original_checkout = if detached_rewrite {
        let repository = Repository::discover(repo).map_err(|error| error.message().to_owned())?;
        let head = repository
            .head()
            .map_err(|error| error.message().to_owned())?;
        if head.is_branch() {
            head.shorthand().map(|name| (false, name.to_owned()))
        } else {
            head.peel_to_commit()
                .ok()
                .map(|commit| (true, commit.id().to_string()))
        }
    } else {
        None
    };
    if detached_rewrite {
        run(repo, &["switch", "--detach", &plan.tip_commit])?;
    }
    let result = if plan.mode == MoveMode::Direct || plan.include_descendants {
        run_explicit_rebase(repo, &plan.replay_plan(), executable)?
    } else {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let order_path =
            env::temp_dir().join(format!("forkstack-{}-{stamp}.todo", std::process::id()));
        fs::write(&order_path, format!("{}\n", plan.commits.join("\n")))
            .map_err(|error| error.to_string())?;
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
        if detached_rewrite {
            run(repo, &["switch", &plan.checkout_branch])?;
        }
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&result.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&result.stdout).trim().to_owned();
        let error = if stderr.is_empty() { stdout } else { stderr };
        if detached_rewrite {
            let state = Repository::discover(repo)
                .map_err(|restore_error| {
                    format!(
                        "{error}\nAdditionally, could not inspect the repository for recovery: {}",
                        restore_error.message()
                    )
                })?
                .state();
            if state == RepositoryState::Clean
                && let Some((was_detached, original)) = original_checkout
            {
                let restore = if was_detached {
                    run(repo, &["switch", "--detach", &original])
                } else {
                    run(repo, &["switch", &original])
                };
                if let Err(restore_error) = restore {
                    return Err(format!(
                        "{error}\nAdditionally, could not restore the original checkout: {restore_error}"
                    ));
                }
            }
        }
        Err(error)
    }
}

#[cfg(feature = "integration-tests")]
#[doc(hidden)]
pub fn apply_move_with_test_executable(
    repo: &Path,
    plan: &MovePlan,
    executable: &Path,
) -> Result<(), String> {
    apply_move_with_executable(repo, plan, executable)
}

pub fn amend_and_restack(repo: &Path, remote: &str, base: &str) -> Result<String, String> {
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    amend_and_restack_with_executable(repo, remote, base, &executable)
}

fn amend_and_restack_with_executable(
    repo: &Path,
    remote: &str,
    base: &str,
    executable: &Path,
) -> Result<String, String> {
    let repository = Repository::discover(repo).map_err(|error| error.message().to_owned())?;
    if repository.state() != RepositoryState::Clean {
        return Err("cannot amend while a Git operation is in progress".into());
    }
    let head = repository
        .head()
        .map_err(|error| error.message().to_owned())?;
    let branch = head
        .shorthand()
        .filter(|branch| branch.starts_with("fs-head/"))
        .ok_or("check out a Forkstack fs-head branch before amending")?
        .to_owned();
    let old_commit = head
        .peel_to_commit()
        .map_err(|error| error.message().to_owned())?;
    let old_message = old_commit.message_bytes().to_owned();
    drop(old_commit);
    drop(head);
    drop(repository);

    let unstaged = output(repo, &["diff", "--quiet"])?;
    if !unstaged.status.success() {
        return Err("cannot amend with unstaged tracked changes; stage or stash them first".into());
    }
    let staged = output(repo, &["diff", "--cached", "--quiet"])?;
    if staged.status.success() {
        return Err("there are no staged changes to amend".into());
    }

    let graph = load_graph_for(repo, remote, base)?;
    run(repo, &["commit", "--amend", "--no-edit"])?;

    let repository = Repository::discover(repo).map_err(|error| error.message().to_owned())?;
    let new_commit = repository
        .head()
        .and_then(|head| head.peel_to_commit())
        .map_err(|error| error.message().to_owned())?;
    if new_commit.message_bytes() != old_message {
        return Err("the amend changed the commit message; descendants were not replayed".into());
    }
    let new_head = new_commit.id().to_string();
    drop(new_commit);
    drop(repository);

    let Some(plan) = graph.plan_descendant_replay(&new_head)? else {
        return Ok(format!("amended {branch}; no descendants to replay"));
    };
    let replayed = plan
        .ref_updates
        .iter()
        .filter(|(_, name)| name.starts_with("fs-head/"))
        .count();
    let result = run_explicit_rebase(repo, &plan, executable)?;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&result.stdout).trim().to_owned();
        return Err(if stderr.is_empty() { stdout } else { stderr });
    }
    run(repo, &["switch", &branch])?;
    Ok(format!(
        "amended {branch} and replayed {replayed} descendant branch(es)"
    ))
}

#[cfg(feature = "integration-tests")]
#[doc(hidden)]
pub fn amend_and_restack_with_test_executable(
    repo: &Path,
    remote: &str,
    base: &str,
    executable: &Path,
) -> Result<String, String> {
    amend_and_restack_with_executable(repo, remote, base, executable)
}

pub(crate) fn displayed_remote_heads(graph: &Graph, remote: &str) -> Vec<String> {
    let prefix = format!("{remote}/fs-head/");
    graph
        .commits
        .values()
        .flat_map(|commit| &commit.remote_refs)
        .filter_map(|name| {
            name.strip_prefix(&prefix)
                .map(|tail| format!("fs-head/{tail}"))
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn load_pr_links(
    options: &crate::core::submit::SubmitOptions,
    heads: &[String],
) -> Result<BTreeMap<String, PullRequestLink>, String> {
    if heads.is_empty() {
        return Ok(BTreeMap::new());
    }
    let fork = crate::core::submit::github_repository(&options.repo, &options.remote)?;
    let found = discover_pr_links(&ProcessRunner, &options.repo, &fork, heads)?;
    Ok(remote_pr_links(&options.remote, found))
}

pub(crate) fn remote_pr_links(
    remote: &str,
    found: BTreeMap<String, PullRequestLink>,
) -> BTreeMap<String, PullRequestLink> {
    found
        .into_iter()
        .filter(|(head, pr)| pr.head_ref_name == *head)
        .map(|(head, pr)| (format!("{remote}/{head}"), pr))
        .collect()
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
    fn graph_roots_include_all_local_branches_and_visible_remote_refs() {
        let remotes = BTreeSet::from(["origin", "upstream"]);
        for name in [
            "refs/heads/fs-head/topic/1",
            "refs/heads/fs-base/topic/1",
            "refs/heads/trunk",
            "refs/heads/unrelated",
            "refs/remotes/origin/fs-head/topic/1",
            "refs/remotes/origin/fs-base/topic/1",
            "refs/remotes/origin/trunk",
            "refs/remotes/upstream/fs-head/topic/1",
            "refs/remotes/upstream/fs-base/topic/1",
            "refs/remotes/upstream/trunk",
        ] {
            assert!(is_graph_root_ref(name, "trunk", &remotes), "{name}");
        }
        for name in [
            "refs/remotes/other/fs-base/topic/1",
            "refs/remotes/upstream/unrelated",
            "refs/tags/v1",
        ] {
            assert!(!is_graph_root_ref(name, "trunk", &remotes), "{name}");
        }
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
    fn clean_repository_ignores_a_stale_rebase_head() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "forkstack-stale-rebase-head-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();

        let repository = Repository::init(&path).unwrap();
        let mut index = repository.index().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("Forkstack Test", "test@example.com").unwrap();
        let commit = repository
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        fs::write(repository.path().join("REBASE_HEAD"), commit.to_string()).unwrap();
        assert_eq!(repository.state(), RepositoryState::Clean);
        drop(tree);
        drop(repository);

        let graph = load_graph(&path).unwrap();
        assert!(
            graph
                .commits
                .values()
                .all(|commit| commit.conflict.is_none())
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn deletes_checked_out_forkstack_branch_locally_and_from_both_remotes() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "forkstack-delete-branch-{}-{stamp}",
            std::process::id()
        ));
        let repo = root.join("repo");
        let origin = root.join("origin.git");
        let upstream = root.join("upstream.git");
        fs::create_dir_all(&root).unwrap();
        run(&root, &["init", "--bare", origin.to_str().unwrap()]).unwrap();
        run(&root, &["init", "--bare", upstream.to_str().unwrap()]).unwrap();
        run(&root, &["init", "-b", "main", repo.to_str().unwrap()]).unwrap();
        run(&repo, &["config", "user.name", "ForkStack Test"]).unwrap();
        run(
            &repo,
            &["config", "user.email", "forkstack@example.invalid"],
        )
        .unwrap();
        fs::write(repo.join("base"), "base\n").unwrap();
        run(&repo, &["add", "base"]).unwrap();
        run(&repo, &["commit", "-m", "base"]).unwrap();
        let base = run(&repo, &["rev-parse", "HEAD"]).unwrap();
        run(&repo, &["switch", "-c", "fs-head/topic/1"]).unwrap();
        fs::write(repo.join("change"), "change\n").unwrap();
        run(&repo, &["add", "change"]).unwrap();
        run(&repo, &["commit", "-m", "change"]).unwrap();
        let head = run(&repo, &["rev-parse", "HEAD"]).unwrap();
        for (remote, path) in [("origin", &origin), ("upstream", &upstream)] {
            run(&repo, &["remote", "add", remote, path.to_str().unwrap()]).unwrap();
            run(
                &repo,
                &[
                    "push",
                    remote,
                    &format!("{head}:refs/heads/fs-head/topic/1"),
                    &format!("{base}:refs/heads/fs-base/topic/1"),
                ],
            )
            .unwrap();
        }

        let status = delete_forkstack_branch(
            &repo,
            &DeleteBranchPlan {
                branch: "fs-head/topic/1".into(),
                expected_head: head,
                checkout_branch: "main".into(),
                expected_checkout: base,
                remote_branches: ["origin", "upstream"]
                    .into_iter()
                    .flat_map(|remote| {
                        ["fs-head/topic/1", "fs-base/topic/1"]
                            .into_iter()
                            .map(move |branch| (remote.into(), branch.into()))
                    })
                    .collect(),
            },
        )
        .unwrap();

        assert_eq!(
            status,
            "deleted fs-head/topic/1 locally and 4 remote branches"
        );
        assert_eq!(run(&repo, &["branch", "--show-current"]).unwrap(), "main");
        assert!(
            run(
                &repo,
                &["show-ref", "--verify", "refs/heads/fs-head/topic/1"]
            )
            .is_err()
        );
        for remote in ["origin", "upstream"] {
            assert!(
                run(
                    &repo,
                    &[
                        "ls-remote",
                        "--heads",
                        remote,
                        "refs/heads/fs-head/topic/1",
                        "refs/heads/fs-base/topic/1",
                    ],
                )
                .unwrap()
                .is_empty()
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn displayed_remote_heads_are_exact_configured_heads_and_deduplicated() {
        let graph = Graph {
            commits: [
                (
                    "a".into(),
                    Commit {
                        remote_refs: vec![
                            "origin/fs-head/topic/2".into(),
                            "upstream/fs-head/other/1".into(),
                            "origin/fs-base/topic/3".into(),
                            "origin/ordinary".into(),
                        ],
                        ..Commit::default()
                    },
                ),
                (
                    "b".into(),
                    Commit {
                        remote_refs: vec![
                            "origin/fs-head/topic/1".into(),
                            "origin/fs-head/topic/2".into(),
                        ],
                        ..Commit::default()
                    },
                ),
            ]
            .into(),
            ..Graph::default()
        };

        assert_eq!(
            displayed_remote_heads(&graph, "origin"),
            ["fs-head/topic/1", "fs-head/topic/2"]
        );
    }

    #[test]
    fn pull_request_links_map_back_to_the_matching_remote_ref() {
        let links = remote_pr_links(
            "upstream",
            [
                (
                    "fs-head/topic/1".into(),
                    PullRequestLink {
                        number: 1,
                        head_ref_name: "fs-head/topic/1".into(),
                        url: "https://example.invalid/1".into(),
                    },
                ),
                (
                    "fs-head/topic/2".into(),
                    PullRequestLink {
                        number: 2,
                        head_ref_name: "fs-head/unexpected".into(),
                        url: "https://example.invalid/2".into(),
                    },
                ),
            ]
            .into(),
        );

        assert_eq!(
            links.keys().cloned().collect::<Vec<_>>(),
            ["upstream/fs-head/topic/1"]
        );
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
            mode: MoveMode::Reorder,
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
            explicit_rebase_todo(&plan.replay_plan()),
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
