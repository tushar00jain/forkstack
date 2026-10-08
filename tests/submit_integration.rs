use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use forkstack::core::submit::test_support;
use forkstack::core::submit::{
    SubmitOptions, assign_branches, execute_local, execute_with, identity_from_message, plan,
};
use forkstack::integrations::{CommandRunner, ProcessRunner};
use forkstack::ui::git::{
    amend_and_restack_with_test_executable, apply_move, apply_move_with_test_executable, checkout,
    load_graph_for, reset_local_heads,
};
use forkstack::ui::model::{MoveMode, MovePlan};
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

fn add_linear_forkstack_branches(fixture: &Fixture) -> (String, String) {
    git(
        &fixture.repo,
        &["branch", "fs-head/draft/1", &fixture.first],
    );
    git(
        &fixture.repo,
        &["branch", "fs-head/draft/2", &fixture.second],
    );
    git(&fixture.repo, &["switch", "-c", "fs-head/rdma/2"]);
    fs::write(fixture.repo.join("rdma-two"), "two\n").unwrap();
    git(&fixture.repo, &["add", "rdma-two"]);
    git(&fixture.repo, &["commit", "-m", "rdma two"]);
    let rdma2 = git(&fixture.repo, &["rev-parse", "HEAD"]);
    git(&fixture.repo, &["switch", "-c", "fs-head/rdma/3"]);
    fs::write(fixture.repo.join("rdma-three"), "three\n").unwrap();
    git(&fixture.repo, &["add", "rdma-three"]);
    git(&fixture.repo, &["commit", "-m", "rdma three"]);
    let rdma3 = git(&fixture.repo, &["rev-parse", "HEAD"]);
    git(&fixture.repo, &["branch", "-f", "main", &fixture.base]);
    (rdma2, rdma3)
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
fn cli_preparation_uses_only_targeted_fetches() {
    let fixture = fixture();
    git(
        &fixture.remote,
        &["update-ref", "refs/heads/unrelated", &fixture.second],
    );
    git(
        &fixture.remote,
        &["update-ref", "refs/tags/unrelated", &fixture.second],
    );
    let runner = CountingRunner::default();

    let prepared =
        test_support::prepare_with_runner(options(&fixture.repo), None, &runner).unwrap();

    assert_eq!(prepared.commits.len(), 2);
    assert!(!ref_exists(&fixture.repo, "refs/remotes/origin/unrelated"));
    assert!(!ref_exists(&fixture.repo, "refs/tags/unrelated"));
    let calls = runner.calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        2,
        "two remote comparisons and no unchanged ref fetches: {calls:?}"
    );
    assert_eq!(calls[0][0], "ls-remote");
    assert_eq!(calls[1][0], "ls-remote");
    for call in calls.iter().filter(|call| call[0] == "fetch") {
        assert!(call.contains(&"--no-tags".into()), "{call:?}");
        assert!(
            call.iter().any(|arg| arg.starts_with("+refs/heads/")),
            "targeted fetch lacks a refspec: {call:?}"
        );
    }
    assert!(
        calls
            .iter()
            .all(|call| !call.contains(&"refs/heads/unrelated".into()))
    );
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

#[test]
fn plan_defaults_prefix_to_fork_owner() {
    let fixture = fixture();
    git(
        &fixture.repo,
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:octocat/example.git",
        ],
    );
    let mut submit_options = options(&fixture.repo);
    submit_options.prefix = None;

    let submit = plan(submit_options).unwrap();

    assert_eq!(submit.options.prefix.as_deref(), Some("octocat"));
    assert_eq!(submit.commits[0].branch, "octocat/1");
    assert_eq!(submit.commits[1].branch, "octocat/2");
}

#[test]
fn local_only_records_identities_and_creates_no_remote_refs() {
    let fixture = fixture();

    execute_local(plan(options(&fixture.repo)).unwrap()).unwrap();

    for (branch, identity) in [
        ("fs-head/draft/1", "draft/1"),
        ("fs-head/draft/2", "draft/2"),
    ] {
        assert!(ref_exists(&fixture.repo, branch));
        assert!(
            git(&fixture.repo, &["show", "-s", "--format=%B", branch])
                .contains(&format!("fs-branch: {identity}"))
        );
        assert!(
            git(
                &fixture.repo,
                &[
                    "ls-remote",
                    "--heads",
                    "origin",
                    &format!("refs/heads/{branch}"),
                ],
            )
            .is_empty()
        );
    }
}

#[test]
fn cli_deletes_checked_out_and_lower_stack_layers() {
    let fixture = fixture();
    git(&fixture.repo, &["switch", "-c", "fs-head/delete/1"]);
    fs::write(fixture.repo.join("delete-one"), "one\n").unwrap();
    git(&fixture.repo, &["add", "delete-one"]);
    git(&fixture.repo, &["commit", "-m", "delete one"]);
    let first = git(&fixture.repo, &["rev-parse", "HEAD"]);
    git(&fixture.repo, &["switch", "-c", "fs-head/delete/2"]);
    fs::write(fixture.repo.join("delete-two"), "two\n").unwrap();
    git(&fixture.repo, &["add", "delete-two"]);
    git(&fixture.repo, &["commit", "-m", "delete two"]);
    let second = git(&fixture.repo, &["rev-parse", "HEAD"]);
    git(&fixture.repo, &["switch", "-c", "fs-head/delete/3"]);
    fs::write(fixture.repo.join("delete-three"), "three\n").unwrap();
    git(&fixture.repo, &["add", "delete-three"]);
    git(&fixture.repo, &["commit", "-m", "delete three"]);
    git(
        &fixture.repo,
        &[
            "push",
            "origin",
            &format!("{}:refs/heads/fs-base/delete/1", fixture.second),
            &format!("{first}:refs/heads/fs-head/delete/1"),
            &format!("{first}:refs/heads/fs-base/delete/2"),
            &format!("{second}:refs/heads/fs-head/delete/2"),
        ],
    );
    fetch_all(&fixture.repo);
    let binary = env!("CARGO_BIN_EXE_forkstack");

    let preview = Command::new(binary)
        .args([
            "delete",
            "fs-head/delete/2",
            "--substack",
            "--repo",
            fixture.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(preview.status.success());
    let stdout = String::from_utf8_lossy(&preview.stdout);
    assert!(stdout.contains("fs-head/delete/1"));
    assert!(stdout.contains("fs-head/delete/2"));
    assert!(stdout.contains("Dry run. Re-run with --execute to apply."));
    assert!(ref_exists(&fixture.repo, "refs/heads/fs-head/delete/1"));
    assert!(ref_exists(&fixture.repo, "refs/heads/fs-head/delete/2"));

    let execute = Command::new(binary)
        .args([
            "delete",
            "fs-head/delete/2",
            "--substack",
            "--execute",
            "--repo",
            fixture.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        execute.status.success(),
        "{}",
        String::from_utf8_lossy(&execute.stderr)
    );
    assert_eq!(
        git(&fixture.repo, &["branch", "--show-current"]),
        "fs-head/delete/3"
    );
    assert!(ref_exists(&fixture.repo, "refs/heads/fs-head/delete/3"));
    for identity in ["delete/1", "delete/2"] {
        assert!(!ref_exists(
            &fixture.repo,
            &format!("refs/heads/fs-head/{identity}")
        ));
        for kind in ["fs-head", "fs-base"] {
            assert!(
                git(
                    &fixture.repo,
                    &[
                        "ls-remote",
                        "--heads",
                        "origin",
                        &format!("refs/heads/{kind}/{identity}"),
                    ],
                )
                .is_empty()
            );
        }
    }
}

#[test]
fn amend_preserves_messages_and_replays_descendant_branches() {
    let fixture = fixture();
    git(&fixture.repo, &["switch", "-c", "fs-head/amend/1"]);
    fs::write(fixture.repo.join("amend-one"), "one\n").unwrap();
    git(&fixture.repo, &["add", "amend-one"]);
    git(
        &fixture.repo,
        &["commit", "-m", "layer one", "-m", "fs-branch: amend/1"],
    );
    let old_one = git(&fixture.repo, &["rev-parse", "HEAD"]);

    git(&fixture.repo, &["switch", "-c", "fs-head/amend/2"]);
    fs::write(fixture.repo.join("amend-two"), "two\n").unwrap();
    git(&fixture.repo, &["add", "amend-two"]);
    git(
        &fixture.repo,
        &["commit", "-m", "layer two", "-m", "fs-branch: amend/2"],
    );
    let old_two = git(&fixture.repo, &["rev-parse", "HEAD"]);

    git(&fixture.repo, &["switch", "-c", "fs-head/amend/3"]);
    fs::write(fixture.repo.join("amend-three"), "three\n").unwrap();
    git(&fixture.repo, &["add", "amend-three"]);
    git(
        &fixture.repo,
        &["commit", "-m", "layer three", "-m", "fs-branch: amend/3"],
    );
    let old_three = git(&fixture.repo, &["rev-parse", "HEAD"]);
    let messages = [
        git(
            &fixture.repo,
            &["show", "-s", "--format=%B", "fs-head/amend/1"],
        ),
        git(
            &fixture.repo,
            &["show", "-s", "--format=%B", "fs-head/amend/2"],
        ),
        git(
            &fixture.repo,
            &["show", "-s", "--format=%B", "fs-head/amend/3"],
        ),
    ];

    git(&fixture.repo, &["switch", "fs-head/amend/1"]);
    fs::write(fixture.repo.join("amend-one"), "one amended\n").unwrap();
    git(&fixture.repo, &["add", "amend-one"]);

    let status = amend_and_restack_with_test_executable(
        &fixture.repo,
        "origin",
        "main",
        Path::new(env!("CARGO_BIN_EXE_forkstack")),
    )
    .unwrap();

    let new_one = git(&fixture.repo, &["rev-parse", "fs-head/amend/1"]);
    let new_two = git(&fixture.repo, &["rev-parse", "fs-head/amend/2"]);
    let new_three = git(&fixture.repo, &["rev-parse", "fs-head/amend/3"]);
    assert_ne!(new_one, old_one);
    assert_ne!(new_two, old_two);
    assert_ne!(new_three, old_three);
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/amend/2^"]),
        new_one
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/amend/3^"]),
        new_two
    );
    for (branch, message) in [
        ("fs-head/amend/1", &messages[0]),
        ("fs-head/amend/2", &messages[1]),
        ("fs-head/amend/3", &messages[2]),
    ] {
        assert_eq!(
            git(&fixture.repo, &["show", "-s", "--format=%B", branch]),
            *message
        );
        assert!(
            git(
                &fixture.repo,
                &[
                    "ls-remote",
                    "--heads",
                    "origin",
                    &format!("refs/heads/{branch}")
                ],
            )
            .is_empty()
        );
    }
    assert_eq!(
        git(&fixture.repo, &["show", "fs-head/amend/1:amend-one"]),
        "one amended"
    );
    assert_eq!(
        git(&fixture.repo, &["branch", "--show-current"]),
        "fs-head/amend/1"
    );
    assert_eq!(
        status,
        "amended fs-head/amend/1 and replayed 2 descendant branch(es)"
    );
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
    stack: Option<(u64, Vec<u64>)>,
    pushes: Vec<Vec<String>>,
    bases_at_push: Vec<BTreeMap<String, String>>,
    bases_at_link: Vec<BTreeMap<String, String>>,
    events: Vec<String>,
}

#[derive(Default)]
struct FakeRunner {
    state: Mutex<FakeState>,
}

fn graphql_values(query: &str, field: &str) -> Vec<String> {
    query
        .match_indices(field)
        .map(|(index, _)| {
            serde_json::Deserializer::from_str(&query[index + field.len()..])
                .into_iter::<String>()
                .next()
                .unwrap()
                .unwrap()
        })
        .collect()
}

fn fake_pr_json(pr: &FakePr, head: &str) -> serde_json::Value {
    json!({
        "id": format!("PR_{}", pr.number),
        "number": pr.number,
        "baseRefName": pr.base,
        "headRefName": head,
        "title": pr.title,
        "body": pr.body,
        "isDraft": pr.draft,
        "url": format!("https://github.com/example/repo/pull/{}", pr.number),
    })
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
                let bases = state
                    .prs
                    .iter()
                    .map(|(head, pr)| (head.clone(), pr.base.clone()))
                    .collect();
                state.pushes.push(args.to_vec());
                state.bases_at_push.push(bases);
                state.events.push("git:push".into());
            }
            return ProcessRunner.run(program, args, repo, input, env);
        }
        assert_eq!(program, "gh");
        let mut state = self.state.lock().unwrap();
        if args.first().map(String::as_str) == Some("api")
            && args
                .get(1)
                .is_some_and(|arg| arg.contains("/stacks?pull_request="))
        {
            assert!(input.is_none());
            assert!(env.contains_key("GH_REPO"));
            let pull_request = args[1].rsplit_once('=').unwrap().1.parse::<u64>().unwrap();
            return Ok(match &state.stack {
                Some((number, pull_requests)) if pull_requests.contains(&pull_request) => json!([{
                    "number": number,
                    "pull_requests": pull_requests,
                }])
                .to_string(),
                _ => "[]".into(),
            });
        }
        if args.starts_with(&["stack".into(), "unstack".into()]) {
            assert!(input.is_none());
            assert!(env.contains_key("GH_REPO"));
            state.stack = None;
            state.events.push("gh:unstack".into());
            return Ok(String::new());
        }
        if args.starts_with(&["stack".into(), "link".into()]) {
            assert!(input.is_none());
            assert!(env.contains_key("GH_REPO"));
            let base_index = args.iter().position(|arg| arg == "--base").unwrap();
            let branch_start = base_index + 2;
            let branches = &args[branch_start..];
            let bases = state
                .prs
                .iter()
                .map(|(head, pr)| (head.clone(), pr.base.clone()))
                .collect();
            state.bases_at_link.push(bases);
            for (index, branch) in branches.iter().enumerate() {
                state.prs.get_mut(branch).unwrap().base = if index == 0 {
                    args[base_index + 1].clone()
                } else {
                    branches[index - 1].clone()
                };
            }
            state.stack = Some((
                7,
                branches
                    .iter()
                    .map(|branch| state.prs[branch].number)
                    .collect(),
            ));
            state.events.push("gh:link".into());
            return Ok(String::new());
        }
        assert_eq!(args.get(1).map(String::as_str), Some("graphql"));
        assert_eq!(args, ["api", "graphql", "--input", "-"]);
        let request: serde_json::Value = serde_json::from_str(input.unwrap()).unwrap();
        let query = request["query"].as_str().unwrap().to_owned();
        if query.starts_with("query(") {
            state.events.push("gh:discover".into());
            let heads = graphql_values(&query, "headRefName:");
            let mut repository = serde_json::Map::from_iter([("id".into(), json!("R_repo"))]);
            for (index, head) in heads.iter().enumerate() {
                let nodes = state
                    .prs
                    .get(head)
                    .map(|pr| vec![fake_pr_json(pr, head)])
                    .unwrap_or_default();
                repository.insert(format!("p{index}"), json!({"nodes": nodes}));
            }
            Ok(json!({"data": {"repository": repository}}).to_string())
        } else if query.contains("createPullRequest") {
            state.events.push("gh:create".into());
            let bases = graphql_values(&query, "baseRefName:");
            let heads = graphql_values(&query, "headRefName:");
            let titles = graphql_values(&query, "title:");
            let bodies = graphql_values(&query, "body:");
            let draft = query.contains("draft:true");
            let mut data = serde_json::Map::new();
            for index in 0..heads.len() {
                let number = 101 + state.prs.len() as u64;
                let pr = FakePr {
                    number,
                    base: bases[index].clone(),
                    title: titles[index].clone(),
                    body: bodies[index].clone(),
                    draft,
                };
                data.insert(
                    format!("p{index}"),
                    json!({"pullRequest": fake_pr_json(&pr, &heads[index])}),
                );
                state.prs.insert(heads[index].clone(), pr);
            }
            Ok(json!({"data": data}).to_string())
        } else if query.contains("updatePullRequest") {
            state.events.push("gh:edit".into());
            let ids = graphql_values(&query, "pullRequestId:");
            let bodies = graphql_values(&query, "body:");
            let bases = graphql_values(&query, "baseRefName:");
            assert!(bases.is_empty() || bases.len() == ids.len());
            let mut data = serde_json::Map::new();
            for index in 0..ids.len() {
                let (head, pr) = state
                    .prs
                    .iter_mut()
                    .find(|(_, pr)| format!("PR_{}", pr.number) == ids[index])
                    .unwrap();
                pr.body = bodies[index].clone();
                if let Some(base) = bases.get(index) {
                    pr.base = base.clone();
                }
                data.insert(
                    format!("p{index}"),
                    json!({"pullRequest": fake_pr_json(pr, head)}),
                );
            }
            Ok(json!({"data": data}).to_string())
        } else {
            panic!("unexpected gh GraphQL invocation: {args:?}")
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
fn branch_assignment_advances_past_local_and_remote_identities() {
    let fixture = fixture();
    git(
        &fixture.repo,
        &["update-ref", "refs/heads/fs-head/draft/4", &fixture.base],
    );
    git(
        &fixture.repo,
        &[
            "update-ref",
            "refs/remotes/origin/fs-head/draft/7",
            &fixture.base,
        ],
    );

    let assigned = assign_branches(
        &fixture.repo,
        &[fixture.first.clone(), fixture.second.clone()],
        "origin",
        Some("draft"),
    )
    .unwrap();

    assert_eq!(
        assigned
            .iter()
            .map(|step| step.branch.as_str())
            .collect::<Vec<_>>(),
        ["draft/8", "draft/9"]
    );
}

#[test]
fn tagged_stack_planning_uses_no_git_processes() {
    let fixture = fixture();
    let options = options(&fixture.repo);
    let mut initial = plan(options.clone()).unwrap();
    test_support::rewrite_with_identities(&mut initial).unwrap();

    let runner = CountingRunner::default();
    let plan = test_support::plan_with_runner(options, &runner).unwrap();
    assert!(plan.commits.iter().all(|step| !step.identity_added));
    let calls = runner.calls.lock().unwrap();
    assert!(calls.is_empty(), "unexpected Git commands: {calls:?}");
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
    assert!(calls.is_empty(), "unexpected Git commands: {calls:?}");
}

#[test]
fn graph_walks_all_local_branch_history() {
    let fixture = fixture();
    git(&fixture.repo, &["switch", "--orphan", "unrelated"]);
    git(
        &fixture.repo,
        &["commit", "--allow-empty", "-m", "unrelated root"],
    );
    let unrelated = git(&fixture.repo, &["rev-parse", "HEAD"]);
    git(&fixture.repo, &["tag", "unrelated-tag", &unrelated]);
    git(&fixture.repo, &["switch", "main"]);
    git(&fixture.repo, &["tag", "visible-tag", &fixture.first]);
    git(
        &fixture.repo,
        &["update-ref", "refs/remotes/upstream/main", &fixture.base],
    );
    git(
        &fixture.repo,
        &[
            "update-ref",
            "refs/remotes/upstream/fs-head/draft/1",
            &fixture.first,
        ],
    );
    git(
        &fixture.repo,
        &[
            "update-ref",
            "refs/remotes/upstream/fs-base/ignored",
            &fixture.second,
        ],
    );

    let graph = forkstack::ui::git::load_graph_for(&fixture.repo, "origin", "main").unwrap();
    assert!(graph.commits.contains_key(&unrelated));
    assert!(
        graph.commits[&unrelated]
            .local_refs
            .contains(&"unrelated".into())
    );
    assert!(
        graph.commits[&unrelated]
            .tags
            .contains(&"unrelated-tag".into())
    );
    assert!(graph.commits.contains_key(&fixture.second));
    assert!(graph.commits.contains_key(&fixture.base));
    assert!(
        graph.commits[&fixture.first]
            .tags
            .contains(&"visible-tag".into())
    );
    assert!(
        graph.commits[&fixture.first]
            .remote_refs
            .contains(&"upstream/fs-head/draft/1".into())
    );
    assert!(
        graph.commits[&fixture.base]
            .remote_refs
            .contains(&"upstream/main".into())
    );
    assert!(
        graph.commits[&fixture.second]
            .remote_refs
            .contains(&"upstream/fs-base/ignored".into())
    );
}

#[test]
fn graph_stops_after_the_visible_roots_common_merge_base() {
    let fixture = fixture();
    git(
        &fixture.repo,
        &["update-ref", "refs/remotes/origin/main", &fixture.first],
    );

    let graph = load_graph_for(&fixture.repo, "origin", "main").unwrap();

    assert!(graph.commits.contains_key(&fixture.first));
    assert!(graph.commits.contains_key(&fixture.second));
    assert!(!graph.commits.contains_key(&fixture.base));
}

#[test]
fn graph_reads_conflicted_paths_from_the_index() {
    let fixture = fixture();
    git(
        &fixture.repo,
        &["switch", "-c", "conflict-side", &fixture.base],
    );
    fs::write(fixture.repo.join("shared"), "side\n").unwrap();
    git(&fixture.repo, &["add", "shared"]);
    git(&fixture.repo, &["commit", "-m", "side conflict"]);
    let incoming = git(&fixture.repo, &["rev-parse", "HEAD"]);

    git(&fixture.repo, &["switch", "main"]);
    fs::write(fixture.repo.join("shared"), "local\n").unwrap();
    git(&fixture.repo, &["add", "shared"]);
    git(&fixture.repo, &["commit", "-m", "local conflict"]);
    let local = git(&fixture.repo, &["rev-parse", "HEAD"]);
    let merge = Command::new("git")
        .args(["merge", "conflict-side"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(!merge.status.success());

    let graph = forkstack::ui::git::load_graph_for(&fixture.repo, "origin", "main").unwrap();
    assert_eq!(
        graph.commits[&local].conflict.as_deref(),
        Some("local (conflict in progress)")
    );
    assert_eq!(
        graph.commits[&incoming].conflict.as_deref(),
        Some("incoming, conflict (merge: shared)")
    );
}

#[test]
fn direct_substack_move_creates_a_parallel_tree() {
    let fixture = fixture();
    let (old_rdma2, old_rdma3) = add_linear_forkstack_branches(&fixture);
    let graph = load_graph_for(&fixture.repo, "origin", "main").unwrap();
    let plan = graph.plan_move(&old_rdma2, &fixture.base, true).unwrap();

    apply_move_with_test_executable(
        &fixture.repo,
        &plan,
        Path::new(env!("CARGO_BIN_EXE_forkstack")),
    )
    .unwrap();

    let new_rdma2 = git(&fixture.repo, &["rev-parse", "fs-head/rdma/2"]);
    let new_rdma3 = git(&fixture.repo, &["rev-parse", "fs-head/rdma/3"]);
    assert_ne!(new_rdma2, old_rdma2);
    assert_ne!(new_rdma3, old_rdma3);
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/rdma/2^"]),
        fixture.base
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/rdma/3^"]),
        new_rdma2
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/draft/2"]),
        fixture.second
    );
    assert_eq!(
        git(&fixture.repo, &["branch", "--show-current"]),
        "fs-head/rdma/3"
    );
}

#[test]
fn cli_previews_reorder_and_executes_substack_move() {
    let fixture = fixture();
    let (old_rdma2, old_rdma3) = add_linear_forkstack_branches(&fixture);
    let binary = env!("CARGO_BIN_EXE_forkstack");

    let preview = Command::new(binary)
        .args([
            "reorder",
            "fs-head/rdma/2",
            "main",
            "--substack",
            "--repo",
            fixture.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let stdout = String::from_utf8_lossy(&preview.stdout);
    assert!(stdout.contains("reorder: fs-head/rdma/2 substack onto main"));
    assert!(stdout.contains("Dry run. Re-run with --execute to apply."));
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/rdma/2"]),
        old_rdma2
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/rdma/3"]),
        old_rdma3
    );

    let execute = Command::new(binary)
        .args([
            "move",
            "fs-head/rdma/2",
            "main",
            "--substack",
            "--execute",
            "--repo",
            fixture.repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        execute.status.success(),
        "{}",
        String::from_utf8_lossy(&execute.stderr)
    );
    let new_rdma2 = git(&fixture.repo, &["rev-parse", "fs-head/rdma/2"]);
    let new_rdma3 = git(&fixture.repo, &["rev-parse", "fs-head/rdma/3"]);
    assert_ne!(new_rdma2, old_rdma2);
    assert_ne!(new_rdma3, old_rdma3);
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/rdma/2^"]),
        fixture.base
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/rdma/3^"]),
        new_rdma2
    );
}

#[test]
fn direct_single_commit_move_leaves_descendants_on_the_original_tree() {
    let fixture = fixture();
    let (old_rdma2, old_rdma3) = add_linear_forkstack_branches(&fixture);
    let graph = load_graph_for(&fixture.repo, "origin", "main").unwrap();
    let plan = graph.plan_move(&old_rdma2, &fixture.base, false).unwrap();

    apply_move_with_test_executable(
        &fixture.repo,
        &plan,
        Path::new(env!("CARGO_BIN_EXE_forkstack")),
    )
    .unwrap();

    let new_rdma2 = git(&fixture.repo, &["rev-parse", "fs-head/rdma/2"]);
    assert_ne!(new_rdma2, old_rdma2);
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/rdma/2^"]),
        fixture.base
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/rdma/3"]),
        old_rdma3
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/rdma/3^"]),
        old_rdma2
    );
    assert_eq!(
        git(&fixture.repo, &["branch", "--show-current"]),
        "fs-head/rdma/2"
    );
}

#[test]
fn substack_reorder_replays_the_destination_tree_on_top() {
    let fixture = fixture();
    let (old_rdma2, _) = add_linear_forkstack_branches(&fixture);
    let graph = load_graph_for(&fixture.repo, "origin", "main").unwrap();
    let plan = graph.plan_reorder(&old_rdma2, &fixture.base, true).unwrap();

    apply_move_with_test_executable(
        &fixture.repo,
        &plan,
        Path::new(env!("CARGO_BIN_EXE_forkstack")),
    )
    .unwrap();

    let new_rdma2 = git(&fixture.repo, &["rev-parse", "fs-head/rdma/2"]);
    let new_rdma3 = git(&fixture.repo, &["rev-parse", "fs-head/rdma/3"]);
    let new_draft1 = git(&fixture.repo, &["rev-parse", "fs-head/draft/1"]);
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/rdma/2^"]),
        fixture.base
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/rdma/3^"]),
        new_rdma2
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/draft/1^"]),
        new_rdma3
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/draft/2^"]),
        new_draft1
    );
    assert_eq!(
        git(&fixture.repo, &["branch", "--show-current"]),
        "fs-head/draft/2"
    );
}

#[test]
fn checkout_lets_git_refuse_a_conflicting_dirty_switch_without_data_loss() {
    let fixture = fixture();
    git(
        &fixture.repo,
        &["switch", "-c", "checkout-target", &fixture.base],
    );
    fs::write(fixture.repo.join("base"), "target contents\n").unwrap();
    git(&fixture.repo, &["add", "base"]);
    git(&fixture.repo, &["commit", "-m", "change base on target"]);
    let target = git(&fixture.repo, &["rev-parse", "HEAD"]);
    git(&fixture.repo, &["switch", "main"]);
    fs::write(fixture.repo.join("base"), "unsaved contents\n").unwrap();

    let error = checkout(&fixture.repo, &target, Some("checkout-target")).unwrap_err();
    assert!(error.contains("local changes"), "{error}");
    assert_eq!(
        fs::read_to_string(fixture.repo.join("base")).unwrap(),
        "unsaved contents\n"
    );
    assert_eq!(git(&fixture.repo, &["branch", "--show-current"]), "main");
}

#[test]
fn detached_apply_restores_branch_when_git_refuses_dirty_rebase() {
    let fixture = fixture();
    fs::write(fixture.repo.join("first"), "unsaved contents\n").unwrap();
    let plan = MovePlan {
        selected: fixture.first.clone(),
        destination: fixture.base.clone(),
        include_descendants: false,
        base: fixture.base.clone(),
        source_base: fixture.base.clone(),
        carried_count: 2,
        mode: MoveMode::Reorder,
        tip: "main".into(),
        tip_commit: fixture.second.clone(),
        detach_for_rewrite: true,
        checkout_branch: "main".into(),
        ref_updates: Vec::new(),
        commits: vec![fixture.first.clone(), fixture.second.clone()],
    };

    let error = apply_move(&fixture.repo, &plan).unwrap_err();
    assert!(error.contains("unstaged changes"), "{error}");
    assert_eq!(
        fs::read_to_string(fixture.repo.join("first")).unwrap(),
        "unsaved contents\n"
    );
    assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD"]), fixture.second);
    assert_eq!(git(&fixture.repo, &["branch", "--show-current"]), "main");
}

#[test]
fn detached_apply_preserves_a_real_rebase_conflict() {
    let fixture = fixture();
    fs::write(fixture.repo.join("shared"), "base\n").unwrap();
    git(&fixture.repo, &["add", "shared"]);
    git(&fixture.repo, &["commit", "-m", "shared base"]);
    let base = git(&fixture.repo, &["rev-parse", "HEAD"]);
    fs::write(fixture.repo.join("shared"), "first\n").unwrap();
    git(&fixture.repo, &["commit", "-am", "shared first"]);
    let first = git(&fixture.repo, &["rev-parse", "HEAD"]);
    fs::write(fixture.repo.join("shared"), "second\n").unwrap();
    git(&fixture.repo, &["commit", "-am", "shared second"]);
    let second = git(&fixture.repo, &["rev-parse", "HEAD"]);
    let plan = MovePlan {
        selected: first.clone(),
        destination: base.clone(),
        include_descendants: false,
        base,
        source_base: fixture.second.clone(),
        carried_count: 2,
        mode: MoveMode::Reorder,
        tip: "main".into(),
        tip_commit: second.clone(),
        detach_for_rewrite: true,
        checkout_branch: "main".into(),
        ref_updates: Vec::new(),
        commits: vec![second, first],
    };

    let error = apply_move_with_test_executable(
        &fixture.repo,
        &plan,
        Path::new(env!("CARGO_BIN_EXE_forkstack")),
    )
    .unwrap_err();
    assert!(
        error.contains("conflict") || error.contains("could not apply"),
        "{error}"
    );
    assert_eq!(git(&fixture.repo, &["branch", "--show-current"]), "");
    assert_ne!(
        git(&fixture.repo, &["diff", "--name-only", "--diff-filter=U"]),
        ""
    );
    git(&fixture.repo, &["rebase", "--abort"]);
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
    let plan = plan(options(&fixture.repo)).unwrap();
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
    submit.commits[0].branch = "draft/1".into();
    submit.commits.truncate(1);
    submit.commits[0].identity_added = false;
    assert!(
        test_support::sync_local_heads(&submit)
            .unwrap_err()
            .contains("divergent local branch")
    );
}

#[test]
fn explicit_local_reset_matches_fetched_origin_heads() {
    let fixture = fixture();
    for (branch, revision) in [
        ("fs-head/draft/1", fixture.first.as_str()),
        ("fs-head/draft/2", fixture.second.as_str()),
    ] {
        git(
            &fixture.repo,
            &["push", "origin", &format!("{revision}:refs/heads/{branch}")],
        );
    }
    fetch_all(&fixture.repo);
    git(
        &fixture.repo,
        &["branch", "fs-head/draft/1", &fixture.second],
    );
    git(&fixture.repo, &["branch", "fs-head/draft/2", &fixture.base]);
    git(
        &fixture.repo,
        &["branch", "fs-head/missing/1", &fixture.base],
    );
    git(&fixture.repo, &["switch", "fs-head/draft/1"]);

    let status = reset_local_heads(&fixture.repo, "origin").unwrap();

    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/draft/1"]),
        fixture.first
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/draft/2"]),
        fixture.second
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/missing/1"]),
        fixture.base
    );
    assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD"]), fixture.first);
    assert!(!fixture.repo.join("second").exists());
    assert_eq!(
        status,
        "reset 2 local branches to origin; skipped 1 without a matching remote branch"
    );
}

#[test]
fn local_reset_refuses_uncommitted_tracked_changes() {
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
    fs::write(fixture.repo.join("first"), "uncommitted\n").unwrap();

    let error = reset_local_heads(&fixture.repo, "origin").unwrap_err();

    assert!(error.contains("uncommitted tracked changes"), "{error}");
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/draft/1"]),
        fixture.second
    );
}

#[test]
fn head_checked_out_in_another_worktree_is_not_overwritten() {
    let fixture = fixture();
    git(
        &fixture.repo,
        &[
            "push",
            "origin",
            &format!("{}:refs/heads/fs-head/draft/1", fixture.base),
        ],
    );
    fetch_all(&fixture.repo);
    git(&fixture.repo, &["branch", "fs-head/draft/1", &fixture.base]);
    let worktree = fixture.root.join("linked-worktree");
    git(
        &fixture.repo,
        &[
            "worktree",
            "add",
            worktree.to_str().unwrap(),
            "fs-head/draft/1",
        ],
    );

    let mut submit = plan(options(&fixture.repo)).unwrap();
    submit.commits[0].branch = "draft/1".into();
    submit.commits.truncate(1);
    submit.commits[0].identity_added = false;
    let error = test_support::sync_local_heads(&submit).unwrap_err();
    assert!(error.contains("checked out in a worktree"), "{error}");
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/draft/1"]),
        fixture.base
    );
}

#[test]
fn existing_pr_with_a_different_base_is_relinked() {
    let fixture = fixture();
    let runner = FakeRunner::default();
    runner.state.lock().unwrap().prs.insert(
        "fs-head/draft/1".into(),
        FakePr {
            number: 101,
            base: "wrong-base".into(),
            title: "manually edited title".into(),
            body: "manual description".into(),
            draft: true,
        },
    );
    let mut submit = plan(options(&fixture.repo)).unwrap();
    execute_with(&mut submit, &runner, &mut |_| {}).unwrap();

    let state = runner.state.lock().unwrap();
    assert_eq!(state.prs["fs-head/draft/1"].base, "main");
    assert_eq!(state.prs["fs-head/draft/2"].base, "fs-head/draft/1");
    assert_eq!(state.prs["fs-head/draft/1"].title, "manually edited title");
    assert!(
        state.prs["fs-head/draft/1"]
            .body
            .starts_with("manual description\n\n<!-- forkstack:stack:start -->")
    );
    assert_eq!(state.events.last().map(String::as_str), Some("gh:link"));
}

#[test]
fn single_pull_request_is_unstacked_before_targeting_base() {
    let fixture = fixture();
    git(&fixture.repo, &["switch", "--detach", &fixture.first]);
    let runner = FakeRunner::default();
    {
        let mut state = runner.state.lock().unwrap();
        state.prs.insert(
            "fs-head/draft/1".into(),
            FakePr {
                number: 101,
                base: "fs-base/draft/1".into(),
                title: "manually edited title".into(),
                body: "manual description".into(),
                draft: true,
            },
        );
        state.stack = Some((7, vec![101, 102]));
    }

    let mut submit = plan(options(&fixture.repo)).unwrap();
    assert_eq!(submit.commits.len(), 1);
    execute_with(&mut submit, &runner, &mut |_| {}).unwrap();

    let state = runner.state.lock().unwrap();
    assert_eq!(state.prs["fs-head/draft/1"].base, "main");
    assert_eq!(state.prs["fs-head/draft/1"].title, "manually edited title");
    assert!(state.stack.is_none());
    assert_eq!(
        state.events,
        ["gh:discover", "git:push", "gh:unstack", "gh:edit"],
        "a single pull request is not a GitHub stack"
    );
}

#[test]
fn execute_creates_and_then_restacks_pull_requests() {
    let fixture = fixture();
    let runner = FakeRunner::default();
    let options = options(&fixture.repo);

    let mut first_plan = plan(options.clone()).unwrap();
    let published_links =
        test_support::execute_with_links(&mut first_plan, &runner, &mut |_| {}).unwrap();
    assert_eq!(
        published_links.keys().cloned().collect::<Vec<_>>(),
        ["fs-head/draft/1", "fs-head/draft/2"]
    );
    assert_eq!(published_links["fs-head/draft/1"].number, 101);
    assert_eq!(
        published_links["fs-head/draft/2"].url,
        "https://github.com/example/repo/pull/102"
    );
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
    assert_eq!(
        identity_from_message(&git(
            &fixture.repo,
            &["show", "-s", "--format=%B", &seeded[1]]
        ))
        .unwrap(),
        Some("draft/2".into())
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/draft/1"]),
        seeded[0]
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/draft/2"]),
        seeded[1]
    );
    assert_eq!(
        git(&fixture.repo, &["config", "branch.fs-head/draft/1.remote"]),
        "origin"
    );
    assert_eq!(
        git(&fixture.repo, &["config", "branch.fs-head/draft/1.merge"]),
        "refs/heads/fs-head/draft/1"
    );
    {
        let state = runner.state.lock().unwrap();
        assert_eq!(state.prs.len(), 2);
        let first = &state.prs["fs-head/draft/1"];
        let second = &state.prs["fs-head/draft/2"];
        assert_eq!(first.base, "main");
        assert_eq!(second.base, "fs-head/draft/1");
        assert_eq!(first.title, "first change");
        assert_eq!(second.title, "second change");
        assert!(first.body.contains("Stack (top to bottom):"));
        assert!(first.body.contains("- -> #101 first change"));
        assert!(second.body.contains("- -> #102 second change"));
        assert!(first.draft && second.draft);
        assert_eq!(state.pushes.len(), 1);
        assert!(state.bases_at_push[0].is_empty());
        assert_eq!(
            state.bases_at_link[0],
            BTreeMap::from([
                ("fs-head/draft/1".into(), "fs-base/draft/1".into(),),
                ("fs-head/draft/2".into(), "fs-base/draft/2".into(),),
            ]),
            "new PRs must initially target stable base refs"
        );
        assert!(state.pushes[0].contains(&"--atomic".into()));
        assert!(state.pushes[0].contains(&"--force-with-lease".into()));
        assert_eq!(
            state.events,
            ["gh:discover", "git:push", "gh:create", "gh:edit", "gh:link",]
        );
    }

    git(&fixture.repo, &["switch", "--detach", &fixture.base]);
    git(&fixture.repo, &["cherry-pick", &seeded[1]]);
    let reordered_second = git(&fixture.repo, &["rev-parse", "HEAD"]);
    git(&fixture.repo, &["cherry-pick", &seeded[0]]);
    let reordered_first = git(&fixture.repo, &["rev-parse", "HEAD"]);

    let mut second_plan = plan(options.clone()).unwrap();
    execute_with(&mut second_plan, &runner, &mut |_| {}).unwrap();
    fetch_all(&fixture.repo);
    let first_head = git(&fixture.repo, &["rev-parse", "origin/fs-head/draft/1"]);
    let second_head = git(&fixture.repo, &["rev-parse", "origin/fs-head/draft/2"]);
    let first_base = git(&fixture.repo, &["rev-parse", "origin/fs-base/draft/1"]);
    let second_base = git(&fixture.repo, &["rev-parse", "origin/fs-base/draft/2"]);
    assert_eq!(first_head, reordered_first);
    assert_eq!(second_head, reordered_second);
    assert_eq!(first_base, reordered_second);
    assert_eq!(second_base, fixture.base);
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/draft/1"]),
        reordered_first
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "fs-head/draft/2"]),
        reordered_second
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "origin/draft/1"]),
        fixture.first
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", "origin/draft/2"]),
        fixture.second
    );
    assert_eq!(
        git(
            &fixture.repo,
            &["diff", "--binary", &first_base, &first_head]
        ),
        git(
            &fixture.repo,
            &["diff", "--binary", &reordered_second, &reordered_first]
        )
    );
    assert_eq!(
        git(
            &fixture.repo,
            &["diff", "--binary", &second_base, &second_head]
        ),
        git(
            &fixture.repo,
            &["diff", "--binary", &fixture.base, &reordered_second]
        )
    );
    let state = runner.state.lock().unwrap();
    assert_eq!(state.prs.len(), 2);
    assert_eq!(state.prs["fs-head/draft/1"].base, "fs-head/draft/2");
    assert_eq!(state.prs["fs-head/draft/2"].base, "main");
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
        state.bases_at_push[1],
        BTreeMap::from([
            ("fs-head/draft/1".into(), "fs-base/draft/1".into(),),
            ("fs-head/draft/2".into(), "fs-base/draft/2".into(),),
        ]),
        "PRs must target stable base refs while rewritten heads are pushed"
    );
    assert_eq!(state.bases_at_link[1], state.bases_at_push[1]);
    assert_eq!(
        &state.events[5..],
        [
            "gh:discover",
            "gh:unstack",
            "gh:edit",
            "git:push",
            "gh:edit",
            "gh:link",
        ]
    );
    drop(state);

    let mut unchanged_plan = plan(options).unwrap();
    execute_with(&mut unchanged_plan, &runner, &mut |_| {}).unwrap();
    let state = runner.state.lock().unwrap();
    assert_eq!(
        &state.events[11..],
        [
            "gh:discover",
            "gh:unstack",
            "gh:edit",
            "git:push",
            "gh:link",
        ],
        "an unchanged existing stack uses one batched GitHub read and relinks the stack"
    );
}

#[test]
fn removed_layer_is_unstacked_before_the_remaining_prs_are_relinked() {
    let fixture = fixture();
    let runner = FakeRunner::default();
    let options = options(&fixture.repo);

    let mut initial = plan(options.clone()).unwrap();
    execute_with(&mut initial, &runner, &mut |_| {}).unwrap();
    {
        let mut state = runner.state.lock().unwrap();
        state.stack.as_mut().unwrap().1.insert(1, 999);
        state.events.clear();
    }

    let mut updated = plan(options).unwrap();
    execute_with(&mut updated, &runner, &mut |_| {}).unwrap();

    let state = runner.state.lock().unwrap();
    assert_eq!(
        state.events,
        [
            "gh:discover",
            "gh:unstack",
            "gh:edit",
            "git:push",
            "gh:link",
        ]
    );
    assert_eq!(
        state.stack.as_ref().unwrap().1,
        state.prs.values().map(|pr| pr.number).collect::<Vec<_>>()
    );
}
