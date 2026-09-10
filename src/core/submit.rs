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
    pub root: String,
    pub commits: Vec<StackCommit>,
    pub updates: Vec<RefUpdate>,
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    integrations::git::run(&ProcessRunner, repo, args)
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

fn remote_identities(repo: &Path, remote: &str, prefix: &str) -> Result<BTreeSet<String>, String> {
    let pattern = format!("refs/remotes/{remote}/fs-head/{prefix}/");
    Ok(git(
        repo,
        &["for-each-ref", "--format=%(refname:strip=4)", &pattern],
    )?
    .lines()
    .map(str::to_owned)
    .collect())
}

pub fn assign_branches(
    repo: &Path,
    revs: &[String],
    remote: &str,
    prefix: Option<&str>,
) -> Result<Vec<StackCommit>, String> {
    let mut records = Vec::new();
    let mut claimed = BTreeSet::new();
    let mut previous: Option<String> = None;
    for rev in revs {
        let parents: Vec<_> = commit_field(repo, rev, "%P")?
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        if parents.len() != 1 {
            return Err(format!(
                "commit {} has {} parents; forkstack requires a linear stack",
                &rev[..rev.len().min(12)],
                parents.len()
            ));
        }
        if previous.as_ref().is_some_and(|old| &parents[0] != old) {
            return Err("forkstack requires a linear stack".into());
        }
        previous = Some(rev.clone());
        let message = commit_field(repo, rev, "%B")?;
        let branch = identity_from_message(&message)?;
        if let Some(branch) = branch.as_ref() {
            if !claimed.insert(branch.clone()) {
                return Err(format!("duplicate {IDENTITY_TRAILER}: {branch}"));
            }
            if git(repo, &["check-ref-format", "--branch", branch]).is_err() {
                return Err(format!("invalid {IDENTITY_TRAILER}: {branch}"));
            }
        }
        records.push((rev.clone(), message, branch));
    }
    if prefix.is_none() && records.iter().any(|(_, _, branch)| branch.is_none()) {
        return Err("untagged commits require --prefix".into());
    }

    let mut known = claimed.clone();
    let mut next_number = 1_u64;
    if let Some(prefix) = prefix {
        known.extend(remote_identities(repo, remote, prefix)?);
        next_number = known
            .iter()
            .filter_map(|branch| branch.strip_prefix(&format!("{prefix}/"))?.parse().ok())
            .max()
            .unwrap_or(0)
            + 1;
    }
    let mut result = Vec::new();
    for (index, (rev, message, existing)) in records.into_iter().enumerate() {
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
            subject: commit_field(repo, &rev, "%s")?,
            body: body_without_identity(&commit_field(repo, &rev, "%b")?),
            rev,
            branch,
            message,
            identity_added,
        });
    }
    Ok(result)
}

pub fn plan(options: SubmitOptions) -> Result<SubmitPlan, String> {
    let url = git(&options.repo, &["remote", "get-url", &options.remote])?;
    let fork = parse_owner_repo(&url).ok_or_else(|| {
        format!(
            "could not read owner/name from the {:?} remote URL",
            options.remote
        )
    })?;
    let base_ref = format!("{}/{}", options.remote, options.base);
    let revs: Vec<String> = git(
        &options.repo,
        &["rev-list", "--reverse", &format!("{base_ref}..HEAD")],
    )?
    .split_whitespace()
    .map(str::to_owned)
    .collect();
    if revs.is_empty() {
        return Err(format!("no commits in {base_ref}..HEAD"));
    }
    let commits = assign_branches(
        &options.repo,
        &revs,
        &options.remote,
        options.prefix.as_deref(),
    )?;
    let root = git(&options.repo, &["rev-parse", &base_ref])?;
    let mut updates = Vec::new();
    for (index, step) in commits.iter().enumerate() {
        updates.push(RefUpdate {
            branch: step.base_branch(),
            rev: if index == 0 {
                root.clone()
            } else {
                commits[index - 1].rev.clone()
            },
        });
        updates.push(RefUpdate {
            branch: step.head_branch(),
            rev: step.rev.clone(),
        });
    }
    Ok(SubmitPlan {
        options,
        fork,
        base_ref,
        root,
        commits,
        updates,
    })
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
        let old_parent = git(repo, &["rev-parse", &format!("{old_rev}^")])?;
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
                &["commit-tree".into(), fields[0].into(), "-p".into(), parent],
                &format!("{}\n", message.trim_end()),
                &env,
            )?
        };
        step.rev = new_rev.clone();
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
    for (index, step) in plan.commits.iter().enumerate() {
        plan.updates.push(RefUpdate {
            branch: step.base_branch(),
            rev: if index == 0 {
                plan.root.clone()
            } else {
                plan.commits[index - 1].rev.clone()
            },
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
    integrations::git::fetch(
        &ProcessRunner,
        &expected.options.repo,
        &expected.options.remote,
    )?;
    let fresh = plan(expected.options.clone())?;
    if &fresh != expected {
        return Err("publish plan changed after fetch; press f to preview the fresh plan".into());
    }
    execute_silent(fresh)
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
    use serde_json::json;
    use std::fs;
    use std::process::Command;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        root: PathBuf,
        repo: PathBuf,
        base: String,
        first: String,
        second: String,
    }

    #[derive(Clone, Debug)]
    struct FakePr {
        number: u64,
        base: String,
        title: String,
        body: String,
        draft: bool,
    }

    #[derive(Default)]
    struct FakeState {
        prs: BTreeMap<String, FakePr>,
        pushes: Vec<Vec<String>>,
        events: Vec<String>,
    }

    #[derive(Default)]
    struct FakeRunner {
        state: Mutex<FakeState>,
    }

    fn option(args: &[String], name: &str) -> String {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|index| args.get(index + 1))
            .unwrap_or_else(|| panic!("missing {name} in {args:?}"))
            .clone()
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            repo: &Path,
            input: Option<&str>,
            env: &BTreeMap<String, String>,
        ) -> Result<String, String> {
            if program == "git" {
                if args.first().is_some_and(|arg| arg == "push") {
                    let mut state = self.state.lock().unwrap();
                    state.pushes.push(args.to_vec());
                    state.events.push("git:push".into());
                }
                return ProcessRunner.run(program, args, repo, input, env);
            }
            assert_eq!(program, "gh");
            let mut state = self.state.lock().unwrap();
            state.events.push(format!("gh:{}", args[1]));
            match args.get(1).map(String::as_str) {
                Some("list") => {
                    let head = option(args, "--head");
                    Ok(state.prs.get(&head).map_or_else(
                        || "[]".into(),
                        |pr| {
                            json!([{
                                "number": pr.number,
                                "baseRefName": pr.base,
                                "title": pr.title,
                                "body": pr.body,
                            }])
                            .to_string()
                        },
                    ))
                }
                Some("create") => {
                    let head = option(args, "--head");
                    let number = 101 + state.prs.len() as u64;
                    state.prs.insert(
                        head,
                        FakePr {
                            number,
                            base: option(args, "--base"),
                            title: option(args, "--title"),
                            body: option(args, "--body"),
                            draft: args.iter().any(|arg| arg == "--draft"),
                        },
                    );
                    Ok(format!("https://github.com/example/repo/pull/{number}"))
                }
                Some("edit") => {
                    let number: u64 = args[2].parse().unwrap();
                    let pr = state
                        .prs
                        .values_mut()
                        .find(|pr| pr.number == number)
                        .unwrap();
                    pr.title = option(args, "--title");
                    pr.body = option(args, "--body");
                    Ok(String::new())
                }
                other => panic!("unexpected gh invocation {other:?}: {args:?}"),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_git(repo: &Path, args: &[&str]) -> String {
        let result = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8_lossy(&result.stdout).trim().to_owned()
    }

    fn fixture() -> Fixture {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "forkstack-rust-test-{}-{stamp}",
            std::process::id()
        ));
        let repo = root.join("repo");
        let remote = root.join("remote.git");
        fs::create_dir_all(&root).unwrap();
        test_git(&root, &["init", "--bare", remote.to_str().unwrap()]);
        test_git(&root, &["init", "-b", "main", repo.to_str().unwrap()]);
        test_git(&repo, &["config", "user.name", "Fork Stack"]);
        test_git(&repo, &["config", "user.email", "forkstack@example.com"]);
        test_git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        let commit = |name: &str, contents: &str, message: &str| {
            fs::write(repo.join(name), contents).unwrap();
            test_git(&repo, &["add", name]);
            test_git(&repo, &["commit", "-m", message]);
            test_git(&repo, &["rev-parse", "HEAD"])
        };
        let base = commit("base", "base\n", "base");
        let first = commit("first", "first\n", "first change");
        let second = commit("second", "second\n", "second change");
        test_git(
            &repo,
            &["push", "origin", &format!("{base}:refs/heads/main")],
        );
        test_git(
            &repo,
            &["push", "origin", &format!("{first}:refs/heads/draft/1")],
        );
        test_git(
            &repo,
            &["push", "origin", &format!("{second}:refs/heads/draft/2")],
        );
        test_git(
            &repo,
            &["fetch", "origin", "+refs/heads/*:refs/remotes/origin/*"],
        );
        Fixture {
            root,
            repo,
            base,
            first,
            second,
        }
    }

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
    fn new_commits_use_remote_positional_identities() {
        let fixture = fixture();
        let revs = vec![fixture.first.clone(), fixture.second.clone()];
        let plan = assign_branches(&fixture.repo, &revs, "origin", Some("draft")).unwrap();
        assert_eq!(
            plan.iter()
                .map(|step| step.branch.as_str())
                .collect::<Vec<_>>(),
            ["draft/1", "draft/2"]
        );
        assert!(plan.iter().all(|step| step.identity_added));
    }

    #[test]
    fn identity_follows_change_when_reordered() {
        let fixture = fixture();
        let mut plan = SubmitPlan {
            options: SubmitOptions {
                repo: fixture.repo.clone(),
                remote: "origin".into(),
                base: "main".into(),
                prefix: Some("draft".into()),
                draft: true,
            },
            fork: "test/remote".into(),
            base_ref: "origin/main".into(),
            root: fixture.base.clone(),
            commits: assign_branches(
                &fixture.repo,
                &[fixture.first.clone(), fixture.second.clone()],
                "origin",
                Some("draft"),
            )
            .unwrap(),
            updates: Vec::new(),
        };
        rewrite_with_identities(&mut plan).unwrap();
        let first = plan.commits[0].rev.clone();
        let second = plan.commits[1].rev.clone();
        test_git(&fixture.repo, &["switch", "--detach", &fixture.base]);
        test_git(&fixture.repo, &["cherry-pick", &second]);
        let reordered_second = test_git(&fixture.repo, &["rev-parse", "HEAD"]);
        test_git(&fixture.repo, &["cherry-pick", &first]);
        let reordered_first = test_git(&fixture.repo, &["rev-parse", "HEAD"]);
        let reordered = assign_branches(
            &fixture.repo,
            &[reordered_second, reordered_first],
            "origin",
            Some("draft"),
        )
        .unwrap();
        assert_eq!(
            reordered
                .iter()
                .map(|step| step.branch.as_str())
                .collect::<Vec<_>>(),
            ["draft/2", "draft/1"]
        );
        assert!(reordered.iter().all(|step| !step.identity_added));
    }

    #[test]
    fn untagged_commits_require_prefix() {
        let fixture = fixture();
        let error =
            assign_branches(&fixture.repo, &[fixture.first.clone()], "origin", None).unwrap_err();
        assert!(error.contains("untagged commits require --prefix"));
    }

    #[test]
    fn mixed_identities_are_preserved_and_new_number_is_unused() {
        let fixture = fixture();
        test_git(
            &fixture.repo,
            &[
                "commit",
                "--allow-empty",
                "-m",
                "imported change\n\nfs-branch: other/6",
            ],
        );
        let imported = test_git(&fixture.repo, &["rev-parse", "HEAD"]);
        test_git(
            &fixture.repo,
            &["commit", "--allow-empty", "-m", "new change"],
        );
        let new = test_git(&fixture.repo, &["rev-parse", "HEAD"]);
        let plan = assign_branches(
            &fixture.repo,
            &[fixture.first.clone(), fixture.second.clone(), imported, new],
            "origin",
            Some("draft"),
        )
        .unwrap();
        assert_eq!(
            plan.iter()
                .map(|step| step.branch.as_str())
                .collect::<Vec<_>>(),
            ["draft/1", "draft/2", "other/6", "draft/3"]
        );
        assert_eq!(
            plan.iter()
                .map(|step| step.identity_added)
                .collect::<Vec<_>>(),
            [true, true, false, true]
        );
    }

    #[test]
    fn refuses_to_overwrite_divergent_local_head_branch() {
        let fixture = fixture();
        test_git(
            &fixture.repo,
            &[
                "push",
                "origin",
                &format!("{}:refs/heads/fs-head/draft/1", fixture.first),
            ],
        );
        test_git(
            &fixture.repo,
            &["fetch", "origin", "+refs/heads/*:refs/remotes/origin/*"],
        );
        test_git(
            &fixture.repo,
            &["branch", "fs-head/draft/1", &fixture.second],
        );
        let plan = SubmitPlan {
            options: SubmitOptions {
                repo: fixture.repo.clone(),
                remote: "origin".into(),
                base: "main".into(),
                prefix: Some("draft".into()),
                draft: true,
            },
            fork: "test/remote".into(),
            base_ref: "origin/main".into(),
            root: fixture.base.clone(),
            commits: vec![StackCommit {
                rev: fixture.first.clone(),
                branch: "draft/1".into(),
                subject: "first change".into(),
                body: String::new(),
                message: "first change".into(),
                identity_added: false,
            }],
            updates: Vec::new(),
        };
        assert!(
            sync_local_heads(&plan)
                .unwrap_err()
                .contains("divergent local branch")
        );
    }

    #[test]
    fn execute_creates_fresh_prs_then_restacks_them() {
        let fixture = fixture();
        let runner = FakeRunner::default();
        let options = SubmitOptions {
            repo: fixture.repo.clone(),
            remote: "origin".into(),
            base: "main".into(),
            prefix: Some("draft".into()),
            draft: true,
        };

        let mut first_plan = plan(options.clone()).unwrap();
        execute_with(&mut first_plan, &runner, &mut |_| {}).unwrap();
        test_git(
            &fixture.repo,
            &["fetch", "origin", "+refs/heads/*:refs/remotes/origin/*"],
        );
        let seeded: Vec<_> = test_git(
            &fixture.repo,
            &["rev-list", "--reverse", "origin/main..HEAD"],
        )
        .lines()
        .map(str::to_owned)
        .collect();
        assert_eq!(seeded.len(), 2);
        assert_eq!(
            identity_from_message(&test_git(
                &fixture.repo,
                &["show", "-s", "--format=%B", &seeded[0]]
            ))
            .unwrap(),
            Some("draft/1".into())
        );
        assert_eq!(
            identity_from_message(&test_git(
                &fixture.repo,
                &["show", "-s", "--format=%B", &seeded[1]]
            ))
            .unwrap(),
            Some("draft/2".into())
        );
        assert_eq!(
            test_git(&fixture.repo, &["rev-parse", "fs-head/draft/1"]),
            seeded[0]
        );
        assert_eq!(
            test_git(&fixture.repo, &["rev-parse", "fs-head/draft/2"]),
            seeded[1]
        );
        assert_eq!(
            test_git(&fixture.repo, &["config", "branch.fs-head/draft/1.remote"]),
            "origin"
        );
        assert_eq!(
            test_git(&fixture.repo, &["config", "branch.fs-head/draft/1.merge"]),
            "refs/heads/fs-head/draft/1"
        );

        {
            let state = runner.state.lock().unwrap();
            assert_eq!(state.prs.len(), 2);
            let first = &state.prs["fs-head/draft/1"];
            let second = &state.prs["fs-head/draft/2"];
            assert_eq!(first.base, "fs-base/draft/1");
            assert_eq!(second.base, "fs-base/draft/2");
            assert_eq!(first.title, "first change");
            assert_eq!(second.title, "second change");
            assert!(first.body.contains("Stack (top to bottom):"));
            assert!(first.body.contains("- -> #101 first change"));
            assert!(second.body.contains("- -> #102 second change"));
            assert!(first.draft && second.draft);
            assert_eq!(state.pushes.len(), 1);
            assert!(state.pushes[0].contains(&"--atomic".into()));
            assert!(state.pushes[0].contains(&"--force-with-lease".into()));
            assert_eq!(
                state.events,
                [
                    "gh:list",
                    "gh:list",
                    "git:push",
                    "gh:create",
                    "gh:create",
                    "gh:edit",
                    "gh:edit"
                ]
            );
        }

        test_git(&fixture.repo, &["switch", "--detach", &fixture.base]);
        test_git(&fixture.repo, &["cherry-pick", &seeded[1]]);
        let reordered_second = test_git(&fixture.repo, &["rev-parse", "HEAD"]);
        test_git(&fixture.repo, &["cherry-pick", &seeded[0]]);
        let reordered_first = test_git(&fixture.repo, &["rev-parse", "HEAD"]);

        let mut second_plan = plan(options).unwrap();
        execute_with(&mut second_plan, &runner, &mut |_| {}).unwrap();
        test_git(
            &fixture.repo,
            &["fetch", "origin", "+refs/heads/*:refs/remotes/origin/*"],
        );
        let first_head = test_git(&fixture.repo, &["rev-parse", "origin/fs-head/draft/1"]);
        let second_head = test_git(&fixture.repo, &["rev-parse", "origin/fs-head/draft/2"]);
        let first_base = test_git(&fixture.repo, &["rev-parse", "origin/fs-base/draft/1"]);
        let second_base = test_git(&fixture.repo, &["rev-parse", "origin/fs-base/draft/2"]);
        assert_eq!(first_head, reordered_first);
        assert_eq!(second_head, reordered_second);
        assert_eq!(first_base, reordered_second);
        assert_eq!(second_base, fixture.base);
        assert_eq!(
            test_git(&fixture.repo, &["rev-parse", "fs-head/draft/1"]),
            reordered_first
        );
        assert_eq!(
            test_git(&fixture.repo, &["rev-parse", "fs-head/draft/2"]),
            reordered_second
        );
        assert_eq!(
            test_git(&fixture.repo, &["rev-parse", "origin/draft/1"]),
            fixture.first
        );
        assert_eq!(
            test_git(&fixture.repo, &["rev-parse", "origin/draft/2"]),
            fixture.second
        );
        assert_eq!(
            test_git(
                &fixture.repo,
                &["diff", "--binary", &first_base, &first_head]
            ),
            test_git(
                &fixture.repo,
                &["diff", "--binary", &reordered_second, &reordered_first]
            )
        );
        assert_eq!(
            test_git(
                &fixture.repo,
                &["diff", "--binary", &second_base, &second_head]
            ),
            test_git(
                &fixture.repo,
                &["diff", "--binary", &fixture.base, &reordered_second]
            )
        );
        let state = runner.state.lock().unwrap();
        assert_eq!(state.prs.len(), 2);
        assert_eq!(state.prs["fs-head/draft/1"].base, "fs-base/draft/1");
        assert_eq!(state.prs["fs-head/draft/2"].base, "fs-base/draft/2");
        assert_eq!(state.prs["fs-head/draft/1"].title, "first change");
        assert_eq!(state.prs["fs-head/draft/2"].title, "second change");
        assert!(
            state.prs["fs-head/draft/1"]
                .body
                .contains("- -> #101 first change")
        );
        assert!(
            state.prs["fs-head/draft/2"]
                .body
                .contains("- -> #102 second change")
        );
        assert_eq!(state.pushes.len(), 2);
        for branch in [
            "fs-base/draft/1",
            "fs-base/draft/2",
            "fs-head/draft/1",
            "fs-head/draft/2",
        ] {
            assert!(
                state.pushes[1]
                    .iter()
                    .any(|arg| arg.contains(&format!("refs/heads/{branch}")))
            );
        }
        assert!(state.pushes[1].contains(&"--atomic".into()));
        assert!(state.pushes[1].contains(&"--force-with-lease".into()));
        assert_eq!(
            &state.events[7..],
            ["gh:list", "gh:list", "git:push", "gh:edit", "gh:edit"]
        );
    }
}
