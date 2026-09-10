use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use git2::{Oid, Repository, Sort};

use crate::integrations::{self, CommandRunner, ProcessRunner};

pub const IDENTITY_TRAILER: &str = "fs-branch";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitOptions {
    pub repo: PathBuf,
    pub remote: String,
    pub base: String,
    pub prefix: Option<String>,
    pub draft: bool,
}

impl Default for SubmitOptions {
    fn default() -> Self {
        Self {
            repo: PathBuf::from("."),
            remote: "origin".into(),
            base: "main".into(),
            prefix: None,
            draft: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackCommit {
    pub rev: String,
    pub parent: String,
    pub branch: String,
    pub subject: String,
    pub body: String,
    pub message: String,
    pub identity_added: bool,
}

impl StackCommit {
    pub fn head_branch(&self) -> String {
        format!("fs-head/{}", self.branch)
    }

    pub fn base_branch(&self) -> String {
        format!("fs-base/{}", self.branch)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefUpdate {
    pub branch: String,
    pub rev: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitPlan {
    pub options: SubmitOptions,
    pub fork: String,
    pub base_ref: String,
    pub commits: Vec<StackCommit>,
    pub updates: Vec<RefUpdate>,
}

fn parse_owner_repo(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches(".git");
    let (_, tail) = trimmed.rsplit_once([':', '/'])?;
    let prefix = &trimmed[..trimmed.len() - tail.len() - 1];
    let owner = prefix.rsplit([':', '/']).next()?;
    (!owner.is_empty() && !tail.is_empty()).then(|| format!("{owner}/{tail}"))
}

#[derive(Debug)]
struct CommitRecord {
    rev: String,
    parent: String,
    subject: String,
    body: String,
    message: String,
}

fn open_repo(path: &Path) -> Result<Repository, String> {
    Repository::discover(path).map_err(|error| error.message().to_owned())
}

fn commit_record(commit: &git2::Commit<'_>) -> Result<CommitRecord, String> {
    if commit.parent_count() != 1 {
        return Err(format!(
            "commit {} has {} parents; forkstack requires a linear stack",
            &commit.id().to_string()[..12],
            commit.parent_count()
        ));
    }
    let message = String::from_utf8_lossy(commit.message_bytes())
        .trim()
        .to_owned();
    let subject = commit.summary().unwrap_or_default().trim().to_owned();
    let body = commit.body().unwrap_or_default().trim().to_owned();
    Ok(CommitRecord {
        rev: commit.id().to_string(),
        parent: commit
            .parent_id(0)
            .map_err(|error| error.message().to_owned())?
            .to_string(),
        subject,
        body,
        message,
    })
}

fn validate_linear_records(records: &[CommitRecord]) -> Result<(), String> {
    for pair in records.windows(2) {
        if pair[1].parent != pair[0].rev {
            return Err("forkstack requires a linear stack".into());
        }
    }
    Ok(())
}

fn read_commit_range(repo: &Repository, base: Oid, head: Oid) -> Result<Vec<CommitRecord>, String> {
    let mut walk = repo.revwalk().map_err(|error| error.message().to_owned())?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)
        .map_err(|error| error.message().to_owned())?;
    walk.push(head)
        .map_err(|error| error.message().to_owned())?;
    walk.hide(base)
        .map_err(|error| error.message().to_owned())?;
    let mut records = Vec::new();
    for oid in walk {
        let oid = oid.map_err(|error| error.message().to_owned())?;
        let commit = repo
            .find_commit(oid)
            .map_err(|error| error.message().to_owned())?;
        records.push(commit_record(&commit)?);
    }
    validate_linear_records(&records)?;
    Ok(records)
}

fn read_commits(repo: &Repository, revs: &[String]) -> Result<Vec<CommitRecord>, String> {
    if revs.is_empty() {
        return Ok(Vec::new());
    }
    let mut records = Vec::with_capacity(revs.len());
    for rev in revs {
        let object = repo
            .revparse_single(rev)
            .map_err(|error| error.message().to_owned())?;
        let commit = object
            .peel_to_commit()
            .map_err(|error| error.message().to_owned())?;
        records.push(commit_record(&commit)?);
    }
    validate_linear_records(&records)?;
    Ok(records)
}

pub fn identity_from_message(message: &str) -> Result<Option<String>, String> {
    let matches: Vec<_> = message.lines().filter_map(identity_trailer_value).collect();
    if matches.len() > 1 {
        Err(format!(
            "multiple {IDENTITY_TRAILER} trailers in one commit"
        ))
    } else {
        Ok(matches.into_iter().next())
    }
}

fn identity_trailer_value(line: &str) -> Option<String> {
    let value = line.strip_prefix(&format!("{IDENTITY_TRAILER}:"))?.trim();
    (!value.is_empty() && !value.chars().any(char::is_whitespace)).then(|| value.to_owned())
}

pub fn add_identity(message: &str, branch: &str) -> Result<String, String> {
    if identity_from_message(message)?.is_some() {
        Ok(message.to_owned())
    } else {
        Ok(format!(
            "{}\n\n{IDENTITY_TRAILER}: {branch}\n",
            message.trim_end()
        ))
    }
}

pub fn body_without_identity(body: &str) -> String {
    let mut stripped = String::new();
    for line in body.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        if identity_trailer_value(content).is_some() {
            if line.ends_with('\n') {
                stripped.push('\n');
            }
        } else {
            stripped.push_str(line);
        }
    }
    stripped.trim_end().to_owned()
}

fn remote_identities(
    repo: &Repository,
    remote: &str,
    prefix: &str,
) -> Result<BTreeSet<String>, String> {
    let root = format!("refs/remotes/{remote}/fs-head/{prefix}/");
    let glob = format!("{root}*");
    let mut identities = BTreeSet::new();
    for reference in repo
        .references_glob(&glob)
        .map_err(|error| error.message().to_owned())?
    {
        let reference = reference.map_err(|error| error.message().to_owned())?;
        if let Some(name) = reference.name().and_then(|name| name.strip_prefix(&root)) {
            identities.insert(format!("{prefix}/{name}"));
        }
    }
    Ok(identities)
}

fn valid_branch_name(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.starts_with(['-', '/'])
        && !branch.ends_with(['/', '.'])
        && !branch.contains("..")
        && !branch.contains("@{")
        && !branch.contains("//")
        && !branch
            .bytes()
            .any(|byte| byte < b' ' || byte == 0x7f || b" ~^:?*[\\".contains(&byte))
        && branch
            .split('/')
            .all(|component| !component.starts_with('.') && !component.ends_with(".lock"))
}

fn assign_records(
    repo: &Repository,
    records: Vec<CommitRecord>,
    remote: &str,
    prefix: Option<&str>,
) -> Result<Vec<StackCommit>, String> {
    let mut claimed = BTreeSet::new();
    let mut parsed = Vec::new();
    for record in records {
        let branch = identity_from_message(&record.message)?;
        if let Some(branch) = branch.as_ref() {
            if !claimed.insert(branch.clone()) {
                return Err(format!("duplicate {IDENTITY_TRAILER}: {branch}"));
            }
            if !valid_branch_name(branch) {
                return Err(format!("invalid {IDENTITY_TRAILER}: {branch}"));
            }
        }
        parsed.push((record, branch));
    }
    if prefix.is_none() && parsed.iter().any(|(_, branch)| branch.is_none()) {
        return Err("untagged commits require --prefix".into());
    }

    let needs_identity = parsed.iter().any(|(_, branch)| branch.is_none());
    let mut known = claimed.clone();
    let mut next_number = 1_u64;
    if let Some(prefix) = prefix.filter(|_| needs_identity) {
        if !valid_branch_name(prefix) {
            return Err(format!("invalid identity prefix: {prefix}"));
        }
        known.extend(remote_identities(repo, remote, prefix)?);
        next_number = known
            .iter()
            .filter_map(|branch| branch.strip_prefix(&format!("{prefix}/"))?.parse().ok())
            .max()
            .unwrap_or(0)
            + 1;
    }
    let mut result = Vec::new();
    for (index, (record, existing)) in parsed.into_iter().enumerate() {
        let identity_added = existing.is_none();
        let branch = if let Some(branch) = existing {
            branch
        } else {
            let prefix = prefix.unwrap();
            let positional = format!("{prefix}/{}", index + 1);
            let branch = if known.contains(&positional) && !claimed.contains(&positional) {
                positional
            } else {
                while known.contains(&format!("{prefix}/{next_number}")) {
                    next_number += 1;
                }
                let branch = format!("{prefix}/{next_number}");
                next_number += 1;
                branch
            };
            claimed.insert(branch.clone());
            known.insert(branch.clone());
            branch
        };
        result.push(StackCommit {
            rev: record.rev,
            parent: record.parent,
            branch,
            subject: record.subject,
            body: body_without_identity(&record.body),
            message: record.message,
            identity_added,
        });
    }
    Ok(result)
}

pub fn assign_branches(
    repo: &Path,
    revs: &[String],
    remote: &str,
    prefix: Option<&str>,
) -> Result<Vec<StackCommit>, String> {
    let repository = open_repo(repo)?;
    assign_records(
        &repository,
        read_commits(&repository, revs)?,
        remote,
        prefix,
    )
}

pub fn plan(options: SubmitOptions) -> Result<SubmitPlan, String> {
    plan_with(options, &ProcessRunner)
}

fn plan_with(options: SubmitOptions, runner: &dyn CommandRunner) -> Result<SubmitPlan, String> {
    let _ = runner;
    let repo = open_repo(&options.repo)?;
    let remote = repo
        .find_remote(&options.remote)
        .map_err(|error| error.message().to_owned())?;
    let url = remote
        .url()
        .ok_or_else(|| format!("remote {:?} has a non-UTF-8 URL", options.remote))?;
    let fork = parse_owner_repo(&url).ok_or_else(|| {
        format!(
            "could not read owner/name from the {:?} remote URL",
            options.remote
        )
    })?;
    let base_ref = format!("{}/{}", options.remote, options.base);
    let base = repo
        .revparse_single(&format!("refs/remotes/{}/{}", options.remote, options.base))
        .and_then(|object| object.peel_to_commit())
        .map_err(|error| error.message().to_owned())?
        .id();
    let head = repo
        .head()
        .and_then(|head| head.peel_to_commit())
        .map_err(|error| error.message().to_owned())?
        .id();
    let records = read_commit_range(&repo, base, head)?;
    if records.is_empty() {
        return Err(format!("no commits in {base_ref}..HEAD"));
    }
    let commits = assign_records(&repo, records, &options.remote, options.prefix.as_deref())?;
    let mut plan = SubmitPlan {
        options,
        fork,
        base_ref,
        commits,
        updates: Vec::new(),
    };
    refresh_updates(&mut plan);
    Ok(plan)
}

#[cfg(feature = "integration-tests")]
#[doc(hidden)]
pub mod test_support {
    use super::*;

    pub fn plan_with_runner(
        options: SubmitOptions,
        runner: &dyn CommandRunner,
    ) -> Result<SubmitPlan, String> {
        plan_with(options, runner)
    }

    pub fn rewrite_with_identities(plan: &mut SubmitPlan) -> Result<(), String> {
        super::rewrite_with_identities(plan)
    }

    pub fn sync_local_heads(plan: &SubmitPlan) -> Result<(), String> {
        super::sync_local_heads(plan)
    }

    pub fn prepare_with_runner(
        options: SubmitOptions,
        expected: Option<&SubmitPlan>,
        runner: &dyn CommandRunner,
    ) -> Result<SubmitPlan, String> {
        super::prepare_with(options, expected, runner)
    }

    pub fn valid_branch_name(branch: &str) -> bool {
        super::valid_branch_name(branch)
    }
}

fn optional_ref(repo: &Repository, reference: &str) -> Option<Oid> {
    repo.find_reference(reference)
        .ok()
        .and_then(|reference| reference.peel_to_commit().ok())
        .map(|commit| commit.id())
}

fn checked_out_branches(repo: &Repository) -> Result<BTreeSet<String>, String> {
    let mut checked_out = BTreeSet::new();
    if let Ok(head) = repo.head() {
        if let Some(name) = head.name() {
            checked_out.insert(name.to_owned());
        }
    }
    for name in repo
        .worktrees()
        .map_err(|error| error.message().to_owned())?
        .iter()
        .flatten()
    {
        let worktree = repo
            .find_worktree(name)
            .map_err(|error| error.message().to_owned())?;
        let worktree_repo =
            Repository::open(worktree.path()).map_err(|error| error.message().to_owned())?;
        if let Ok(head) = worktree_repo.head() {
            if let Some(name) = head.name() {
                checked_out.insert(name.to_owned());
            }
        }
    }
    Ok(checked_out)
}

fn rewrite_with_identities(plan: &mut SubmitPlan) -> Result<(), String> {
    let repo_path = &plan.options.repo;
    let repo = open_repo(repo_path)?;
    let old_head = repo
        .head()
        .and_then(|head| head.peel_to_commit())
        .map_err(|error| error.message().to_owned())?
        .id();
    let mut new_parent: Option<String> = None;
    let mut rewritten = Vec::new();
    for step in &mut plan.commits {
        let old_rev = step.rev.clone();
        let old_parent = step.parent.clone();
        let parent = new_parent.clone().unwrap_or_else(|| old_parent.clone());
        let message = add_identity(&step.message, &step.branch)?;
        let new_rev = if parent == old_parent && message == step.message {
            old_rev.clone()
        } else {
            let old_commit = repo
                .find_commit(Oid::from_str(&old_rev).map_err(|error| error.message().to_owned())?)
                .map_err(|error| error.message().to_owned())?;
            let tree = old_commit
                .tree()
                .map_err(|error| error.message().to_owned())?;
            let parent_commit = repo
                .find_commit(Oid::from_str(&parent).map_err(|error| error.message().to_owned())?)
                .map_err(|error| error.message().to_owned())?;
            repo.commit(
                None,
                &old_commit.author(),
                &old_commit.committer(),
                &format!("{}\n", message.trim_end()),
                &tree,
                &[&parent_commit],
            )
            .map_err(|error| error.message().to_owned())?
            .to_string()
        };
        step.rev = new_rev.clone();
        step.parent = parent;
        step.message = message.clone();
        step.body = body_without_identity(&message.lines().skip(1).collect::<Vec<_>>().join("\n"));
        new_parent = Some(new_rev.clone());
        rewritten.push((old_rev, new_rev));
    }
    let replacements: BTreeMap<Oid, Oid> = rewritten
        .iter()
        .filter(|(old, new)| old != new)
        .map(|(old, new)| Ok((Oid::from_str(old)?, Oid::from_str(new)?)))
        .collect::<Result<_, git2::Error>>()
        .map_err(|error| error.message().to_owned())?;
    let mut ref_updates = Vec::new();
    for reference in repo
        .references_glob("refs/heads/*")
        .map_err(|error| error.message().to_owned())?
    {
        let reference = reference.map_err(|error| error.message().to_owned())?;
        let (Some(name), Some(old)) = (reference.name(), reference.target()) else {
            continue;
        };
        if let Some(new) = replacements.get(&old) {
            ref_updates.push((name.to_owned(), old, *new));
        }
    }
    let detached_head = repo
        .head_detached()
        .map_err(|error| error.message().to_owned())?;
    if detached_head {
        if let Some(new) = new_parent
            .as_deref()
            .filter(|new| *new != old_head.to_string())
        {
            ref_updates.push((
                "HEAD".into(),
                old_head,
                Oid::from_str(new).map_err(|error| error.message().to_owned())?,
            ));
        }
    }
    if !ref_updates.is_empty() {
        let mut transaction = repo
            .transaction()
            .map_err(|error| error.message().to_owned())?;
        for (name, _, _) in &ref_updates {
            transaction
                .lock_ref(name)
                .map_err(|error| error.message().to_owned())?;
        }
        for (name, old, new) in &ref_updates {
            let current =
                optional_ref(&repo, name).ok_or_else(|| format!("reference {name} disappeared"))?;
            if current != *old {
                return Err(format!("reference {name} changed while rewriting commits"));
            }
            transaction
                .set_target(name, *new, None, "forkstack: record stable identity")
                .map_err(|error| error.message().to_owned())?;
        }
        transaction
            .commit()
            .map_err(|error| error.message().to_owned())?;
    }
    refresh_updates(plan);
    Ok(())
}

fn refresh_updates(plan: &mut SubmitPlan) {
    plan.updates.clear();
    for step in &plan.commits {
        plan.updates.push(RefUpdate {
            branch: step.base_branch(),
            rev: step.parent.clone(),
        });
        plan.updates.push(RefUpdate {
            branch: step.head_branch(),
            rev: step.rev.clone(),
        });
    }
}

fn sync_local_heads(plan: &SubmitPlan) -> Result<(), String> {
    let repo_path = &plan.options.repo;
    let repo = open_repo(repo_path)?;
    let checked_out = checked_out_branches(&repo)?;
    let mut updates = Vec::new();
    for step in &plan.commits {
        let local = format!("refs/heads/{}", step.head_branch());
        let remote = format!(
            "refs/remotes/{}/{}",
            plan.options.remote,
            step.head_branch()
        );
        let wanted = Oid::from_str(&step.rev).map_err(|error| error.message().to_owned())?;
        let local_rev = optional_ref(&repo, &local);
        if local_rev == Some(wanted) {
            continue;
        }
        if checked_out.contains(&local) {
            return Err(format!(
                "cannot update {:?}: it is checked out in a worktree",
                step.head_branch()
            ));
        }
        if let Some(old) = local_rev {
            if optional_ref(&repo, &remote) != Some(old) {
                return Err(format!(
                    "refusing to overwrite divergent local branch {:?}; it does not match {}/{}",
                    step.head_branch(),
                    plan.options.remote,
                    step.head_branch()
                ));
            }
            updates.push((local, Some(old), wanted));
        } else {
            updates.push((local, None, wanted));
        }
    }
    if !updates.is_empty() {
        let mut transaction = repo
            .transaction()
            .map_err(|error| error.message().to_owned())?;
        for (name, _, _) in &updates {
            transaction
                .lock_ref(name)
                .map_err(|error| error.message().to_owned())?;
        }
        for (name, old, wanted) in &updates {
            if optional_ref(&repo, name) != *old {
                return Err(format!("reference {name} changed while preparing updates"));
            }
            transaction
                .set_target(name, *wanted, None, "forkstack: update PR head")
                .map_err(|error| error.message().to_owned())?;
        }
        transaction
            .commit()
            .map_err(|error| error.message().to_owned())?;
    }
    let mut config = repo.config().map_err(|error| error.message().to_owned())?;
    for step in &plan.commits {
        let head = step.head_branch();
        let remote_key = format!("branch.{head}.remote");
        let merge_key = format!("branch.{head}.merge");
        if config.get_string(&remote_key).as_deref() != Ok(plan.options.remote.as_str()) {
            config
                .set_str(&remote_key, &plan.options.remote)
                .map_err(|error| error.message().to_owned())?;
        }
        let merge = format!("refs/heads/{head}");
        if config.get_string(&merge_key).as_deref() != Ok(merge.as_str()) {
            config
                .set_str(&merge_key, &merge)
                .map_err(|error| error.message().to_owned())?;
        }
    }
    Ok(())
}

pub fn stack_table(entries: &[(String, Option<u64>, String)], current: &str) -> String {
    let mut lines = vec![String::new(), "---".into(), "Stack (top to bottom):".into()];
    for (branch, number, subject) in entries.iter().rev() {
        let mark = if branch == current { "->" } else { "" };
        let reference = number
            .map(|n| format!("#{n}"))
            .unwrap_or_else(|| branch.clone());
        lines.push(format!("- {mark} {reference} {subject}"));
    }
    lines.extend(["---".into(), String::new()]);
    lines.join("\n")
}

pub fn execute_checked(expected: &SubmitPlan) -> Result<(), String> {
    let fresh = prepare(expected.options.clone(), Some(expected))?;
    if &fresh != expected {
        return Err("publish plan changed after fetch; press f to preview the fresh plan".into());
    }
    execute_silent(fresh)
}

pub fn prepare(
    options: SubmitOptions,
    expected: Option<&SubmitPlan>,
) -> Result<SubmitPlan, String> {
    prepare_with(options, expected, &ProcessRunner)
}

fn sync_remote_refs(plan: &SubmitPlan, runner: &dyn CommandRunner) -> Result<(), String> {
    let (branches, identity_prefixes) = remote_sync_scope(plan);
    let missing = integrations::git::fetch_remote_branches(
        runner,
        &plan.options.repo,
        &plan.options.remote,
        branches,
        identity_prefixes,
    )?;
    integrations::git::delete_refs(&plan.options.repo, &missing)
}

fn remote_sync_scope(plan: &SubmitPlan) -> (Vec<String>, Vec<String>) {
    let mut branches = vec![plan.options.base.clone()];
    for step in &plan.commits {
        branches.push(step.base_branch());
        branches.push(step.head_branch());
    }
    let identity_prefixes = if plan.commits.iter().any(|step| step.identity_added) {
        plan.options
            .prefix
            .as_ref()
            .map(|prefix| vec![format!("fs-head/{prefix}")])
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    (branches, identity_prefixes)
}

fn prepare_with(
    options: SubmitOptions,
    expected: Option<&SubmitPlan>,
    runner: &dyn CommandRunner,
) -> Result<SubmitPlan, String> {
    if let Some(expected) = expected {
        sync_remote_refs(expected, runner)?;
    } else {
        let missing = integrations::git::fetch_remote_branches(
            runner,
            &options.repo,
            &options.remote,
            [options.base.clone()],
            [],
        )?;
        integrations::git::delete_refs(&options.repo, &missing)?;
        let provisional = plan(options.clone())?;
        sync_remote_refs(&provisional, runner)?;
    }
    plan(options)
}

pub fn execute(mut plan: SubmitPlan) -> Result<(), String> {
    execute_with(&mut plan, &ProcessRunner, &mut |message| {
        println!("{message}")
    })
}

fn execute_silent(mut plan: SubmitPlan) -> Result<(), String> {
    execute_with(&mut plan, &ProcessRunner, &mut |_| {})
}

pub fn execute_with(
    plan: &mut SubmitPlan,
    runner: &dyn CommandRunner,
    report: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let heads: Vec<_> = plan.commits.iter().map(StackCommit::head_branch).collect();
    let mut discovery =
        integrations::github::discover_prs(runner, &plan.options.repo, &plan.fork, &heads)?;
    let mut prs = Vec::with_capacity(plan.commits.len());
    let mut was_existing = Vec::with_capacity(plan.commits.len());
    for step in &plan.commits {
        let pr = discovery.by_head.remove(&step.head_branch());
        if let Some(pr) = pr.as_ref()
            && pr.base_ref_name != step.base_branch()
        {
            return Err(format!(
                "PR #{} targets {:?}; expected {:?}",
                pr.number,
                pr.base_ref_name,
                step.base_branch()
            ));
        }
        was_existing.push(pr.is_some());
        prs.push(pr);
    }
    if plan.commits.iter().any(|step| step.identity_added) {
        report(&format!("recording stable {IDENTITY_TRAILER} trailers"));
        rewrite_with_identities(plan)?;
    }
    report("updating local PR head branches");
    sync_local_heads(plan)?;
    report("pushing PR base and head refs");
    integrations::git::push_atomic(
        runner,
        &plan.options.repo,
        &plan.options.remote,
        plan.updates
            .iter()
            .map(|update| (update.branch.clone(), update.rev.clone())),
    )?;
    let missing: Vec<_> = plan
        .commits
        .iter()
        .enumerate()
        .filter(|(index, _)| prs[*index].is_none())
        .collect();
    let bases: Vec<_> = plan.commits.iter().map(StackCommit::base_branch).collect();
    let create_requests: Vec<_> = missing
        .iter()
        .map(|(index, step)| integrations::github::CreatePullRequest {
            base: &bases[*index],
            head: &heads[*index],
            title: &step.subject,
            body: &step.body,
            draft: plan.options.draft,
        })
        .collect();
    let created = integrations::github::create_prs(
        runner,
        &plan.options.repo,
        &discovery.repository_id,
        &create_requests,
    )?;
    for ((index, _), pr) in missing.into_iter().zip(created) {
        prs[index] = Some(pr);
    }
    for (index, step) in plan.commits.iter().enumerate() {
        if was_existing[index] {
            report(&format!(
                "{}: reusing #{}",
                step.branch,
                prs[index].as_ref().unwrap().number
            ));
        } else {
            report(&format!(
                "{}: created {}",
                step.branch,
                prs[index].as_ref().unwrap().url
            ));
        }
    }
    let entries: Vec<_> = plan
        .commits
        .iter()
        .enumerate()
        .map(|(index, step)| {
            (
                step.branch.clone(),
                prs[index].as_ref().map(|pr| pr.number),
                step.subject.clone(),
            )
        })
        .collect();
    let desired_bodies: Vec<_> = plan
        .commits
        .iter()
        .map(|step| format!("{}{}", stack_table(&entries, &step.branch), step.body))
        .collect();
    let edits: Vec<_> = plan
        .commits
        .iter()
        .enumerate()
        .filter_map(|(index, step)| {
            let pr = prs[index].as_ref()?;
            (pr.title != step.subject || pr.body != desired_bodies[index]).then_some(
                integrations::github::EditPullRequest {
                    id: &pr.id,
                    title: &step.subject,
                    body: &desired_bodies[index],
                },
            )
        })
        .collect();
    integrations::github::edit_prs(runner, &plan.options.repo, &edits)?;
    report(&format!(
        "\n{} pull requests in {}.",
        plan.commits.len(),
        plan.fork
    ));
    Ok(())
}

pub fn print_summary(plan: &SubmitPlan) {
    println!("fork:   {}", plan.fork);
    println!(
        "stack:  {}..HEAD  ({} commits)\n",
        plan.base_ref,
        plan.commits.len()
    );
    for step in &plan.commits {
        let marker = if step.identity_added {
            "  (new identity)"
        } else {
            ""
        };
        println!(
            "  {}  {} ({}) <- base {}{marker}",
            &step.rev[..step.rev.len().min(12)],
            step.branch,
            step.head_branch(),
            step.base_branch()
        );
        println!("                {}", step.subject);
    }
}

pub fn print_plan(plan: &SubmitPlan) {
    print_summary(plan);
    println!("\nDry run. The execute pass would:\n");
    for step in &plan.commits {
        if step.identity_added {
            println!(
                "  add {IDENTITY_TRAILER}: {} to {}",
                step.branch,
                &step.rev[..step.rev.len().min(12)]
            );
        }
        println!(
            "  create/update local {} tracking {}/{}",
            step.head_branch(),
            plan.options.remote,
            step.head_branch()
        );
        println!(
            "  atomically update {} and {}",
            step.base_branch(),
            step.head_branch()
        );
    }
    println!("\nRe-run with --execute to do it.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_identity_and_removes_it_from_body() {
        let message = "subject\n\nbody\n\nfs-branch: draft/2\n";
        assert_eq!(
            identity_from_message(message).unwrap(),
            Some("draft/2".into())
        );
        assert_eq!(
            body_without_identity("body\n\nfs-branch: draft/2\n"),
            "body"
        );
    }

    #[test]
    fn identity_trailers_require_one_non_whitespace_value() {
        for invalid in [
            "subject\n\nfs-branch:\n",
            "subject\n\nfs-branch:   \n",
            "subject\n\nfs-branch: two words\n",
            "subject\n\n fs-branch: draft/1\n",
        ] {
            assert_eq!(identity_from_message(invalid).unwrap(), None);
        }
        assert_eq!(
            body_without_identity("body\nfs-branch: two words\nfs-branch:\n"),
            "body\nfs-branch: two words\nfs-branch:"
        );
        assert_eq!(
            identity_from_message("fs-branch:\tvalid/1  ").unwrap(),
            Some("valid/1".into())
        );
        assert_eq!(
            body_without_identity("before\nfs-branch: valid/1\nafter\n"),
            "before\n\nafter"
        );
    }

    #[test]
    fn stack_table_matches_python_layout() {
        let entries = vec![
            ("a/1".into(), Some(10), "one".into()),
            ("a/2".into(), Some(11), "two".into()),
        ];
        assert!(stack_table(&entries, "a/1").contains("- -> #10 one"));
        assert!(
            stack_table(&entries, "a/1").find("#11").unwrap()
                < stack_table(&entries, "a/1").find("#10").unwrap()
        );
    }

    #[test]
    fn cli_bootstrap_scope_is_limited_to_base_stack_and_active_prefix() {
        let plan = SubmitPlan {
            options: SubmitOptions {
                base: "trunk".into(),
                prefix: Some("topic".into()),
                ..SubmitOptions::default()
            },
            fork: "owner/repo".into(),
            base_ref: "origin/trunk".into(),
            commits: vec![StackCommit {
                rev: "b".into(),
                parent: "a".into(),
                branch: "topic/1".into(),
                subject: "subject".into(),
                body: String::new(),
                message: "subject".into(),
                identity_added: true,
            }],
            updates: Vec::new(),
        };
        let (branches, prefixes) = remote_sync_scope(&plan);
        assert_eq!(branches, ["trunk", "fs-base/topic/1", "fs-head/topic/1"]);
        assert_eq!(prefixes, ["fs-head/topic"]);
    }
}
