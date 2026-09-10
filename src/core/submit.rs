use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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

fn git_with(runner: &dyn CommandRunner, repo: &Path, args: &[&str]) -> Result<String, String> {
    integrations::git::run(runner, repo, args)
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    git_with(&ProcessRunner, repo, args)
}

fn parse_owner_repo(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches(".git");
    let (_, tail) = trimmed.rsplit_once([':', '/'])?;
    let prefix = &trimmed[..trimmed.len() - tail.len() - 1];
    let owner = prefix.rsplit([':', '/']).next()?;
    (!owner.is_empty() && !tail.is_empty()).then(|| format!("{owner}/{tail}"))
}

fn commit_field(repo: &Path, rev: &str, format: &str) -> Result<String, String> {
    git(repo, &["log", "-1", &format!("--format={format}"), rev])
}

#[derive(Debug)]
struct CommitRecord {
    rev: String,
    parent: String,
    subject: String,
    body: String,
    message: String,
}

fn parse_commit_records(output: &str) -> Result<Vec<CommitRecord>, String> {
    let mut records = Vec::new();
    for record in output.split('\x1e') {
        let record = record.trim_matches(['\n', '\r']);
        if record.is_empty() {
            continue;
        }
        let fields: Vec<_> = record.splitn(5, '\x1f').collect();
        if fields.len() != 5 {
            return Err("git log returned an unexpected commit record".into());
        }
        let parents: Vec<_> = fields[1].split_whitespace().collect();
        if parents.len() != 1 {
            return Err(format!(
                "commit {} has {} parents; forkstack requires a linear stack",
                &fields[0][..fields[0].len().min(12)],
                parents.len()
            ));
        }
        records.push(CommitRecord {
            rev: fields[0].trim().to_owned(),
            parent: parents[0].to_owned(),
            subject: fields[2].trim().to_owned(),
            body: fields[3].trim().to_owned(),
            message: fields[4].trim().to_owned(),
        });
    }
    for pair in records.windows(2) {
        if pair[1].parent != pair[0].rev {
            return Err("forkstack requires a linear stack".into());
        }
    }
    Ok(records)
}

fn read_commit_range(
    runner: &dyn CommandRunner,
    repo: &Path,
    range: &str,
) -> Result<Vec<CommitRecord>, String> {
    let format = "--format=%H%x1f%P%x1f%s%x1f%b%x1f%B%x1e";
    parse_commit_records(&git_with(
        runner,
        repo,
        &["log", "--reverse", format, range],
    )?)
}

fn read_commits(
    runner: &dyn CommandRunner,
    repo: &Path,
    revs: &[String],
) -> Result<Vec<CommitRecord>, String> {
    if revs.is_empty() {
        return Ok(Vec::new());
    }
    let format = "--format=%H%x1f%P%x1f%s%x1f%b%x1f%B%x1e";
    let mut args = vec![
        "show".to_owned(),
        "--no-patch".to_owned(),
        "--no-walk=unsorted".to_owned(),
        format.to_owned(),
    ];
    args.extend(revs.iter().cloned());
    parse_commit_records(&integrations::git::run_owned(runner, repo, &args)?)
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
    runner: &dyn CommandRunner,
    repo: &Path,
    remote: &str,
    prefix: &str,
) -> Result<BTreeSet<String>, String> {
    let pattern = format!("refs/remotes/{remote}/fs-head/{prefix}/");
    Ok(git_with(
        runner,
        repo,
        &["for-each-ref", "--format=%(refname:strip=4)", &pattern],
    )?
    .lines()
    .map(str::to_owned)
    .collect())
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
    runner: &dyn CommandRunner,
    repo: &Path,
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
        known.extend(remote_identities(runner, repo, remote, prefix)?);
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
    assign_records(
        &ProcessRunner,
        repo,
        read_commits(&ProcessRunner, repo, revs)?,
        remote,
        prefix,
    )
}

pub fn plan(options: SubmitOptions) -> Result<SubmitPlan, String> {
    plan_with(options, &ProcessRunner)
}

fn plan_with(options: SubmitOptions, runner: &dyn CommandRunner) -> Result<SubmitPlan, String> {
    let url = git_with(
        runner,
        &options.repo,
        &["remote", "get-url", &options.remote],
    )?;
    let fork = parse_owner_repo(&url).ok_or_else(|| {
        format!(
            "could not read owner/name from the {:?} remote URL",
            options.remote
        )
    })?;
    let base_ref = format!("{}/{}", options.remote, options.base);
    let records = read_commit_range(runner, &options.repo, &format!("{base_ref}..HEAD"))?;
    if records.is_empty() {
        return Err(format!("no commits in {base_ref}..HEAD"));
    }
    let commits = assign_records(
        runner,
        &options.repo,
        records,
        &options.remote,
        options.prefix.as_deref(),
    )?;
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

    pub fn sync_remote_refs(plan: &SubmitPlan) -> Result<(), String> {
        super::sync_remote_refs(plan)
    }

    pub fn valid_branch_name(branch: &str) -> bool {
        super::valid_branch_name(branch)
    }
}

fn optional_ref(repo: &Path, reference: &str) -> Option<String> {
    git(repo, &["rev-parse", "--verify", reference]).ok()
}

fn rewrite_with_identities(plan: &mut SubmitPlan) -> Result<(), String> {
    let repo = &plan.options.repo;
    let old_head = git(repo, &["rev-parse", "HEAD"])?;
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
            let fields = commit_field(
                repo,
                &old_rev,
                "%T%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI",
            )?;
            let fields: Vec<_> = fields.split('\0').collect();
            let mut env = BTreeMap::new();
            for (key, value) in [
                ("GIT_AUTHOR_NAME", fields[1]),
                ("GIT_AUTHOR_EMAIL", fields[2]),
                ("GIT_AUTHOR_DATE", fields[3]),
                ("GIT_COMMITTER_NAME", fields[4]),
                ("GIT_COMMITTER_EMAIL", fields[5]),
                ("GIT_COMMITTER_DATE", fields[6]),
            ] {
                env.insert(key.to_owned(), value.to_owned());
            }
            integrations::git::run_input(
                &ProcessRunner,
                repo,
                &[
                    "commit-tree".into(),
                    fields[0].into(),
                    "-p".into(),
                    parent.clone(),
                ],
                &format!("{}\n", message.trim_end()),
                &env,
            )?
        };
        step.rev = new_rev.clone();
        step.parent = parent;
        step.message = message.clone();
        step.body = body_without_identity(&message.lines().skip(1).collect::<Vec<_>>().join("\n"));
        new_parent = Some(new_rev.clone());
        rewritten.push((old_rev, new_rev));
    }
    for (old, new) in &rewritten {
        if old == new {
            continue;
        }
        let refs = git(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "--points-at",
                old,
                "refs/heads/",
            ],
        )?;
        for reference in refs.lines() {
            git(repo, &["update-ref", reference, new, old])?;
        }
    }
    if git(repo, &["rev-parse", "HEAD"])? == old_head
        && new_parent.as_deref() != Some(old_head.as_str())
    {
        git(
            repo,
            &[
                "update-ref",
                "HEAD",
                new_parent.as_ref().unwrap(),
                &old_head,
            ],
        )?;
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
    let repo = &plan.options.repo;
    let checked_out: BTreeSet<_> = git(repo, &["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|line| line.strip_prefix("branch "))
        .map(str::to_owned)
        .collect();
    let mut input = String::new();
    for step in &plan.commits {
        let local = format!("refs/heads/{}", step.head_branch());
        let remote = format!(
            "refs/remotes/{}/{}",
            plan.options.remote,
            step.head_branch()
        );
        let local_rev = optional_ref(repo, &local);
        if local_rev.as_deref() == Some(&step.rev) {
            continue;
        }
        if checked_out.contains(&local) {
            return Err(format!(
                "cannot update {:?}: it is checked out in a worktree",
                step.head_branch()
            ));
        }
        if let Some(old) = local_rev {
            if optional_ref(repo, &remote).as_deref() != Some(&old) {
                return Err(format!(
                    "refusing to overwrite divergent local branch {:?}; it does not match {}/{}",
                    step.head_branch(),
                    plan.options.remote,
                    step.head_branch()
                ));
            }
            input.push_str(&format!("update {local} {} {old}\n", step.rev));
        } else {
            input.push_str(&format!("create {local} {}\n", step.rev));
        }
    }
    if !input.is_empty() {
        integrations::git::run_input(
            &ProcessRunner,
            repo,
            &["update-ref".into(), "--stdin".into()],
            &input,
            &BTreeMap::new(),
        )?;
    }
    for step in &plan.commits {
        let head = step.head_branch();
        git(
            repo,
            &[
                "config",
                &format!("branch.{head}.remote"),
                &plan.options.remote,
            ],
        )?;
        git(
            repo,
            &[
                "config",
                &format!("branch.{head}.merge"),
                &format!("refs/heads/{head}"),
            ],
        )?;
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
    sync_remote_refs(expected)?;
    let fresh = plan(expected.options.clone())?;
    if &fresh != expected {
        return Err("publish plan changed after fetch; press f to preview the fresh plan".into());
    }
    execute_silent(fresh)
}

fn sync_remote_refs(plan: &SubmitPlan) -> Result<(), String> {
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
    integrations::git::fetch_remote_branches(
        &ProcessRunner,
        &plan.options.repo,
        &plan.options.remote,
        branches,
        identity_prefixes,
    )
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
    if plan.commits.iter().any(|step| step.identity_added) {
        report(&format!("recording stable {IDENTITY_TRAILER} trailers"));
        rewrite_with_identities(plan)?;
    }
    report("updating local PR head branches");
    sync_local_heads(plan)?;
    let mut numbers = Vec::new();
    for step in &plan.commits {
        let pr = integrations::github::existing_pr(
            runner,
            &plan.options.repo,
            &plan.fork,
            &step.head_branch(),
        )?;
        if let Some(pr) = pr.as_ref() {
            if pr.base_ref_name != step.base_branch() {
                return Err(format!(
                    "PR #{} targets {:?}; expected {:?}",
                    pr.number,
                    pr.base_ref_name,
                    step.base_branch()
                ));
            }
        }
        numbers.push(pr.map(|pr| pr.number));
    }
    report("pushing PR base and head refs");
    integrations::git::push_atomic(
        runner,
        &plan.options.repo,
        &plan.options.remote,
        plan.updates
            .iter()
            .map(|update| (update.branch.clone(), update.rev.clone())),
    )?;
    for (index, step) in plan.commits.iter().enumerate() {
        if let Some(number) = numbers[index] {
            report(&format!("{}: reusing #{number}", step.branch));
        } else {
            let url = integrations::github::create_pr(
                runner,
                &plan.options.repo,
                &plan.fork,
                &step.base_branch(),
                &step.head_branch(),
                &step.subject,
                &step.body,
                plan.options.draft,
            )?;
            let number = url
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| format!("could not read pull request number from {url:?}"))?;
            report(&format!("{}: created {url}", step.branch));
            numbers[index] = Some(number);
        }
    }
    let entries: Vec<_> = plan
        .commits
        .iter()
        .enumerate()
        .map(|(index, step)| (step.branch.clone(), numbers[index], step.subject.clone()))
        .collect();
    for (index, step) in plan.commits.iter().enumerate() {
        if let Some(number) = numbers[index] {
            integrations::github::edit_pr(
                runner,
                &plan.options.repo,
                &plan.fork,
                number,
                &step.subject,
                &format!("{}{}", stack_table(&entries, &step.branch), step.body),
            )?;
        }
    }
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
}
