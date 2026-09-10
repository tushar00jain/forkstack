use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use forkstack::core::submit::test_support;
use forkstack::core::submit::{
    SubmitOptions, assign_branches, execute_with, identity_from_message, plan,
};
use forkstack::integrations::{CommandRunner, ProcessRunner};
use serde_json::json;

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    remote: PathBuf,
    base: String,
    first: String,
    second: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8_lossy(&result.stdout).trim().to_owned()
}

fn ref_exists(repo: &Path, reference: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", reference])
        .current_dir(repo)
        .status()
        .unwrap()
        .success()
}

fn fixture() -> Fixture {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/integration-fixtures")
        .join(format!("submit-{}-{stamp}", std::process::id()));
    let repo = root.join("repo");
    let remote = root.join("remote.git");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "--bare", remote.to_str().unwrap()]);
    git(&root, &["init", "-b", "main", repo.to_str().unwrap()]);
    git(&repo, &["config", "user.name", "Fork Stack"]);
    git(&repo, &["config", "user.email", "forkstack@example.com"]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let commit = |name: &str, contents: &str, message: &str| {
        fs::write(repo.join(name), contents).unwrap();
        git(&repo, &["add", name]);
        git(&repo, &["commit", "-m", message]);
        git(&repo, &["rev-parse", "HEAD"])
    };
    let base = commit("base", "base\n", "base");
    let first = commit("first", "first\n", "first change");
    let second = commit("second", "second\n", "second change");
    git(
        &repo,
        &["push", "origin", &format!("{base}:refs/heads/main")],
    );
    git(
        &repo,
        &["push", "origin", &format!("{first}:refs/heads/draft/1")],
    );
    git(
        &repo,
        &["push", "origin", &format!("{second}:refs/heads/draft/2")],
    );
    fetch_all(&repo);
    Fixture {
        root,
        repo,
        remote,
        base,
        first,
        second,
    }
}

fn fetch_all(repo: &Path) {
    git(
        repo,
        &["fetch", "origin", "+refs/heads/*:refs/remotes/origin/*"],
    );
}

#[test]
fn checked_execute_fetches_only_the_base_and_affected_stack_refs() {
    let fixture = fixture();
    let expected = plan(options(&fixture.repo)).unwrap();
    let first_step = &expected.commits[0];
    let missing_base = first_step.base_branch();
    let fetched_head = first_step.head_branch();

    git(
        &fixture.repo,
        &[
            "update-ref",
            &format!("refs/remotes/origin/{missing_base}"),
            &fixture.first,
        ],
    );
    git(
        &fixture.repo,
        &["update-ref", "refs/remotes/origin/unrelated", &fixture.base],
    );
    git(
        &fixture.remote,
        &["update-ref", "refs/heads/main", &fixture.first],
    );
    git(
        &fixture.remote,
        &[
            "update-ref",
            &format!("refs/heads/{fetched_head}"),
            &fixture.second,
        ],
    );
    git(
        &fixture.remote,
        &["update-ref", "refs/heads/unrelated", &fixture.second],
    );
    git(
        &fixture.remote,
        &["update-ref", "refs/tags/unrelated", &fixture.second],
    );

    let error = forkstack::core::submit::execute_checked(&expected).unwrap_err();
    assert!(
        error.contains("publish plan changed after fetch"),
        "{error}"
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "refs/remotes/origin/main"]),
        fixture.first
    );
    assert_eq!(
        git(
            &fixture.repo,
            &["rev-parse", &format!("refs/remotes/origin/{fetched_head}")]
        ),
        fixture.second
    );
    assert!(!ref_exists(
        &fixture.repo,
        &format!("refs/remotes/origin/{missing_base}")
    ));
    assert_eq!(
        git(
            &fixture.repo,
            &["rev-parse", "refs/remotes/origin/unrelated"]
        ),
        fixture.base
    );
    assert!(!ref_exists(&fixture.repo, "refs/tags/unrelated"));
}

#[test]
fn checked_execute_detects_a_new_identity_in_the_active_prefix() {
    let fixture = fixture();
    let mut tagged = plan(options(&fixture.repo)).unwrap();
    test_support::rewrite_with_identities(&mut tagged).unwrap();
    git(
        &fixture.repo,
        &["commit", "--allow-empty", "-m", "third change"],
    );
    let expected = plan(options(&fixture.repo)).unwrap();
    assert_eq!(expected.commits.len(), 3);
    assert_eq!(expected.commits[2].branch, "draft/3");
    assert!(expected.commits[2].identity_added);

    git(
        &fixture.repo,
        &[
            "update-ref",
            "refs/remotes/origin/fs-head/other/9",
            &fixture.base,
        ],
    );
    git(
        &fixture.remote,
        &["update-ref", "refs/heads/fs-head/draft/4", &fixture.first],
    );
    git(
        &fixture.remote,
        &["update-ref", "refs/heads/fs-head/other/9", &fixture.first],
    );

    let error = forkstack::core::submit::execute_checked(&expected).unwrap_err();
    assert!(
        error.contains("publish plan changed after fetch"),
        "{error}"
    );
    assert_eq!(
        git(
            &fixture.repo,
            &["rev-parse", "refs/remotes/origin/fs-head/draft/4"]
        ),
        fixture.first
    );
    assert_eq!(
        git(
            &fixture.repo,
            &["rev-parse", "refs/remotes/origin/fs-head/other/9"]
        ),
        fixture.base
    );
    let fresh = plan(options(&fixture.repo)).unwrap();
    assert_eq!(fresh.commits[2].branch, "draft/5");
}

fn advance_remote_main(fixture: &Fixture) -> String {
    git(&fixture.repo, &["switch", "--detach", &fixture.base]);
    fs::write(fixture.repo.join("advanced"), "advanced main\n").unwrap();
    git(&fixture.repo, &["add", "advanced"]);
    git(&fixture.repo, &["commit", "-m", "advanced main"]);
    let advanced = git(&fixture.repo, &["rev-parse", "HEAD"]);
    git(
        &fixture.repo,
        &["push", "origin", &format!("{advanced}:refs/heads/main")],
    );
    fetch_all(&fixture.repo);
    git(&fixture.repo, &["switch", "main"]);
    advanced
}

fn options(repo: &Path) -> SubmitOptions {
    SubmitOptions {
        repo: repo.to_owned(),
        remote: "origin".into(),
        base: "main".into(),
        prefix: Some("draft".into()),
        draft: true,
    }
}

#[derive(Default)]
struct CountingRunner {
    calls: Mutex<Vec<Vec<String>>>,
}

impl CommandRunner for CountingRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        repo: &Path,
        input: Option<&str>,
        env: &BTreeMap<String, String>,
    ) -> Result<String, String> {
        self.calls.lock().unwrap().push(args.to_vec());
        ProcessRunner.run(program, args, repo, input, env)
    }
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

#[test]
fn branch_assignment_reads_real_commit_history() {
    let fixture = fixture();
    let initial = assign_branches(
        &fixture.repo,
        &[fixture.first.clone(), fixture.second.clone()],
        "origin",
        Some("draft"),
    )
    .unwrap();
    assert_eq!(
        initial
            .iter()
            .map(|step| step.branch.as_str())
            .collect::<Vec<_>>(),
        ["draft/1", "draft/2"]
    );
    assert!(initial.iter().all(|step| step.identity_added));
    assert!(
        assign_branches(&fixture.repo, &[fixture.first.clone()], "origin", None)
            .unwrap_err()
            .contains("untagged commits require --prefix")
    );

    git(
        &fixture.repo,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "imported change\n\nfs-branch: other/6",
        ],
    );
    let imported = git(&fixture.repo, &["rev-parse", "HEAD"]);
    git(
        &fixture.repo,
        &["commit", "--allow-empty", "-m", "new change"],
    );
    let new = git(&fixture.repo, &["rev-parse", "HEAD"]);
    let mixed = assign_branches(
        &fixture.repo,
        &[fixture.first.clone(), fixture.second.clone(), imported, new],
        "origin",
        Some("draft"),
    )
    .unwrap();
    assert_eq!(
        mixed
            .iter()
            .map(|step| step.branch.as_str())
            .collect::<Vec<_>>(),
        ["draft/1", "draft/2", "other/6", "draft/3"]
    );
}

#[test]
fn tagged_stack_planning_uses_two_git_processes() {
    let fixture = fixture();
    let options = options(&fixture.repo);
    let mut initial = plan(options.clone()).unwrap();
    test_support::rewrite_with_identities(&mut initial).unwrap();

    let runner = CountingRunner::default();
    let plan = test_support::plan_with_runner(options, &runner).unwrap();
    assert!(plan.commits.iter().all(|step| !step.identity_added));
    let calls = runner.calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "unexpected Git commands: {calls:?}");
    assert_eq!(calls[0][..2], ["remote", "get-url"]);
    assert_eq!(calls[1][0], "log");
}

#[test]
fn planning_process_count_is_independent_of_small_stack_size() {
    let fixture = fixture();
    for index in 0..10 {
        git(
            &fixture.repo,
            &[
                "commit",
                "--allow-empty",
                "-m",
                &format!("extra change {index}"),
            ],
        );
    }
    let runner = CountingRunner::default();
    let plan = test_support::plan_with_runner(options(&fixture.repo), &runner).unwrap();
    assert_eq!(plan.commits.len(), 12);
    let calls = runner.calls.lock().unwrap();
    assert_eq!(calls.len(), 3, "unexpected Git commands: {calls:?}");
    assert_eq!(calls[0][..2], ["remote", "get-url"]);
    assert_eq!(calls[1][0], "log");
    assert_eq!(calls[2][0], "for-each-ref");
}

#[test]
fn in_process_branch_validation_matches_git() {
    let valid = ["draft/1", "topic", "a.b", "a@b", "a-b", "a_b", "@"];
    let invalid = [
        "",
        "-draft",
        "/draft",
        "draft/",
        "draft.",
        "draft..x",
        "draft@{x",
        "draft//x",
        ".draft",
        "draft/.x",
        "draft/x.lock",
        "draft lock",
        "draft~x",
        "draft^x",
        "draft:x",
        "draft?x",
        "draft*x",
        "draft[x",
        "draft\\x",
    ];
    for branch in valid.into_iter().chain(invalid) {
        let git_accepts = Command::new("git")
            .args(["check-ref-format", "--branch", branch])
            .output()
            .unwrap()
            .status
            .success();
        assert_eq!(
            test_support::valid_branch_name(branch),
            git_accepts,
            "{branch}"
        );
    }
}

#[test]
fn identities_survive_a_real_reorder() {
    let fixture = fixture();
    let mut plan = plan(options(&fixture.repo)).unwrap();
    test_support::rewrite_with_identities(&mut plan).unwrap();
    let first = plan.commits[0].rev.clone();
    let second = plan.commits[1].rev.clone();

    git(&fixture.repo, &["switch", "--detach", &fixture.base]);
    git(&fixture.repo, &["cherry-pick", &second]);
    let reordered_second = git(&fixture.repo, &["rev-parse", "HEAD"]);
    git(&fixture.repo, &["cherry-pick", &first]);
    let reordered_first = git(&fixture.repo, &["rev-parse", "HEAD"]);
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
fn diverged_remote_base_keeps_each_stack_heads_actual_parent() {
    let fixture = fixture();
    let advanced = advance_remote_main(&fixture);
    for branch in [
        "fs-base/draft/1",
        "fs-head/draft/1",
        "fs-base/draft/2",
        "fs-head/draft/2",
    ] {
        git(
            &fixture.repo,
            &[
                "update-ref",
                &format!("refs/remotes/origin/{branch}"),
                &advanced,
            ],
        );
    }

    let plan = plan(options(&fixture.repo)).unwrap();
    assert_eq!(plan.base_ref, "origin/main");
    assert_eq!(plan.commits[0].rev, fixture.first);
    assert_eq!(plan.commits[1].rev, fixture.second);
    assert_eq!(plan.updates[0].rev, fixture.base);
    assert_eq!(plan.updates[2].rev, fixture.first);
    for step in &plan.commits {
        let base = plan
            .updates
            .iter()
            .find(|update| update.branch == step.base_branch())
            .unwrap();
        assert_eq!(
            base.rev,
            git(&fixture.repo, &["rev-parse", &format!("{}^", step.rev)])
        );
    }

    let graph = forkstack::ui::git::load_graph(&fixture.repo).unwrap();
    let preview = graph.publish_preview(&plan).unwrap();
    assert!(
        preview.commits[&advanced]
            .remote_refs
            .contains(&"origin/main".into())
    );
    assert!(
        preview.commits[&advanced]
            .remote_refs
            .iter()
            .all(|name| !name.contains("/fs-base/") && !name.contains("/fs-head/"))
    );
    assert!(
        preview.commits[&fixture.first]
            .remote_refs
            .contains(&"origin/fs-head/draft/1".into())
    );
    assert_eq!(
        preview.commits[&fixture.second].parents,
        [fixture.first.clone()]
    );
}

#[test]
fn divergent_local_head_is_not_overwritten() {
    let fixture = fixture();
    git(
        &fixture.repo,
        &[
            "push",
            "origin",
            &format!("{}:refs/heads/fs-head/draft/1", fixture.first),
        ],
    );
    fetch_all(&fixture.repo);
    git(
        &fixture.repo,
        &["branch", "fs-head/draft/1", &fixture.second],
    );
    let mut submit = plan(options(&fixture.repo)).unwrap();
    submit.commits.truncate(1);
    submit.commits[0].identity_added = false;
    assert!(
        test_support::sync_local_heads(&submit)
            .unwrap_err()
            .contains("divergent local branch")
    );
}

#[test]
fn execute_creates_and_then_restacks_pull_requests() {
    let fixture = fixture();
    let runner = FakeRunner::default();
    let options = options(&fixture.repo);

    let mut first_plan = plan(options.clone()).unwrap();
    execute_with(&mut first_plan, &runner, &mut |_| {}).unwrap();
    fetch_all(&fixture.repo);
    let seeded: Vec<_> = git(
        &fixture.repo,
        &["rev-list", "--reverse", "origin/main..HEAD"],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    assert_eq!(seeded.len(), 2);
    assert_eq!(
        identity_from_message(&git(
            &fixture.repo,
            &["show", "-s", "--format=%B", &seeded[0]]
        ))
        .unwrap(),
        Some("draft/1".into())
    );
    {
        let state = runner.state.lock().unwrap();
        assert_eq!(state.prs.len(), 2);
        assert_eq!(state.prs["fs-head/draft/1"].base, "fs-base/draft/1");
        assert_eq!(state.prs["fs-head/draft/2"].base, "fs-base/draft/2");
        assert!(state.prs.values().all(|pr| pr.draft));
        assert_eq!(state.pushes.len(), 1);
        assert!(state.pushes[0].contains(&"--atomic".into()));
        assert!(state.pushes[0].contains(&"--force-with-lease".into()));
    }

    git(&fixture.repo, &["switch", "--detach", &fixture.base]);
    git(&fixture.repo, &["cherry-pick", &seeded[1]]);
    let reordered_second = git(&fixture.repo, &["rev-parse", "HEAD"]);
    git(&fixture.repo, &["cherry-pick", &seeded[0]]);
    let reordered_first = git(&fixture.repo, &["rev-parse", "HEAD"]);

    let mut second_plan = plan(options).unwrap();
    execute_with(&mut second_plan, &runner, &mut |_| {}).unwrap();
    fetch_all(&fixture.repo);
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "origin/fs-head/draft/1"]),
        reordered_first
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "origin/fs-head/draft/2"]),
        reordered_second
    );
    let state = runner.state.lock().unwrap();
    assert_eq!(state.prs.len(), 2);
    assert_eq!(state.pushes.len(), 2);
    assert_eq!(
        &state.events[7..],
        ["gh:list", "gh:list", "git:push", "gh:edit", "gh:edit"]
    );
}
