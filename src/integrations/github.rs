use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use super::CommandRunner;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub base_ref_name: String,
}

pub fn existing_pr(
    runner: &dyn CommandRunner,
    repo: &Path,
    fork: &str,
    branch: &str,
) -> Result<Option<PullRequest>, String> {
    let out = runner.run(
        "gh",
        &[
            "pr".into(),
            "list".into(),
            "--repo".into(),
            fork.into(),
            "--head".into(),
            branch.into(),
            "--state".into(),
            "open".into(),
            "--json".into(),
            "number,baseRefName,title,body".into(),
        ],
        repo,
        None,
        &BTreeMap::new(),
    )?;
    let mut prs: Vec<PullRequest> = serde_json::from_str(if out.is_empty() { "[]" } else { &out })
        .map_err(|error| format!("could not parse gh output: {error}"))?;
    Ok(prs.drain(..).next())
}

pub fn create_pr(
    runner: &dyn CommandRunner,
    repo: &Path,
    fork: &str,
    base: &str,
    head: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<String, String> {
    let mut args = vec![
        "pr".into(),
        "create".into(),
        "--repo".into(),
        fork.into(),
        "--base".into(),
        base.into(),
        "--head".into(),
        head.into(),
        "--title".into(),
        title.into(),
        "--body".into(),
        body.into(),
    ];
    if draft {
        args.push("--draft".into());
    }
    runner.run("gh", &args, repo, None, &BTreeMap::new())
}

pub fn edit_pr(
    runner: &dyn CommandRunner,
    repo: &Path,
    fork: &str,
    number: u64,
    title: &str,
    body: &str,
) -> Result<(), String> {
    runner
        .run(
            "gh",
            &[
                "pr".into(),
                "edit".into(),
                number.to_string(),
                "--repo".into(),
                fork.into(),
                "--title".into(),
                title.into(),
                "--body".into(),
                body.into(),
            ],
            repo,
            None,
            &BTreeMap::new(),
        )
        .map(|_| ())
}
