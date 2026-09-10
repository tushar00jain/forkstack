use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use super::CommandRunner;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub id: String,
    pub number: u64,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub title: String,
    pub body: String,
    pub is_draft: bool,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestDiscovery {
    pub repository_id: String,
    pub by_head: BTreeMap<String, PullRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatePullRequest<'a> {
    pub base: &'a str,
    pub head: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub draft: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditPullRequest<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub body: &'a str,
}

#[derive(Deserialize)]
struct DiscoveryResponse {
    #[serde(default)]
    data: Option<DiscoveryData>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Deserialize)]
struct DiscoveryData {
    repository: DiscoveryRepository,
}

#[derive(Deserialize)]
struct DiscoveryRepository {
    id: String,
    #[serde(flatten)]
    queries: BTreeMap<String, PullRequestNodes>,
}

#[derive(Deserialize)]
struct PullRequestNodes {
    nodes: Vec<PullRequest>,
}

#[derive(Deserialize)]
struct MutationResponse {
    #[serde(default)]
    data: Option<BTreeMap<String, MutationPayload>>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Deserialize)]
struct GraphqlError {
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MutationPayload {
    pull_request: PullRequest,
}

fn graphql_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn ensure_no_graphql_errors(errors: &[GraphqlError]) -> Result<(), String> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "GitHub GraphQL error: {}",
            errors
                .iter()
                .map(|error| error.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        ))
    }
}

fn run_graphql(
    runner: &dyn CommandRunner,
    repo: &Path,
    query: String,
    variables: &[(&str, &str)],
) -> Result<String, String> {
    let mut body = serde_json::Map::from_iter([("query".into(), serde_json::Value::String(query))]);
    let mut graphql_variables = serde_json::Map::new();
    for (name, value) in variables {
        graphql_variables.insert(
            (*name).to_owned(),
            serde_json::Value::String((*value).to_owned()),
        );
    }
    if !graphql_variables.is_empty() {
        body.insert(
            "variables".into(),
            serde_json::Value::Object(graphql_variables),
        );
    }
    runner.run(
        "gh",
        &["api".into(), "graphql".into(), "--input".into(), "-".into()],
        repo,
        Some(&serde_json::Value::Object(body).to_string()),
        &BTreeMap::new(),
    )
}

pub fn discover_prs(
    runner: &dyn CommandRunner,
    repo: &Path,
    fork: &str,
    branches: &[String],
) -> Result<PullRequestDiscovery, String> {
    let (owner, name) = fork
        .split_once('/')
        .ok_or_else(|| format!("invalid GitHub repository {fork:?}"))?;
    let selections = branches
        .iter()
        .enumerate()
        .map(|(index, branch)| {
            format!(
                "p{index}:pullRequests(first:2,states:OPEN,headRefName:{},orderBy:{{field:CREATED_AT,direction:DESC}}){{nodes{{id number baseRefName headRefName title body isDraft url}}}}",
                graphql_string(branch)
            )
        })
        .collect::<String>();
    let query = format!(
        "query($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{id {selections}}}}}"
    );
    let out = run_graphql(runner, repo, query, &[("owner", owner), ("name", name)])?;
    let response: DiscoveryResponse = serde_json::from_str(&out)
        .map_err(|error| format!("could not parse gh output: {error}"))?;
    ensure_no_graphql_errors(&response.errors)?;
    let repository = response
        .data
        .ok_or("gh output omitted GraphQL data")?
        .repository;
    let mut by_head = BTreeMap::new();
    for (index, branch) in branches.iter().enumerate() {
        let mut nodes = repository
            .queries
            .get(&format!("p{index}"))
            .ok_or_else(|| format!("gh output omitted pull request query for {branch:?}"))?
            .nodes
            .clone();
        if let Some(pr) = nodes.drain(..).next() {
            by_head.insert(branch.clone(), pr);
        }
    }
    Ok(PullRequestDiscovery {
        repository_id: repository.id,
        by_head,
    })
}

pub fn create_prs(
    runner: &dyn CommandRunner,
    repo: &Path,
    repository_id: &str,
    requests: &[CreatePullRequest<'_>],
) -> Result<Vec<PullRequest>, String> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let mutations = requests
        .iter()
        .enumerate()
        .map(|(index, request)| {
            format!(
                "p{index}:createPullRequest(input:{{repositoryId:{},baseRefName:{},headRefName:{},title:{},body:{},draft:{}}}){{pullRequest{{id number baseRefName headRefName title body isDraft url}}}}",
                graphql_string(repository_id),
                graphql_string(request.base),
                graphql_string(request.head),
                graphql_string(request.title),
                graphql_string(request.body),
                request.draft,
            )
        })
        .collect::<String>();
    let out = run_graphql(runner, repo, format!("mutation{{{mutations}}}"), &[])?;
    let response: MutationResponse = serde_json::from_str(&out)
        .map_err(|error| format!("could not parse gh output: {error}"))?;
    ensure_no_graphql_errors(&response.errors)?;
    let data = response.data.ok_or("gh output omitted GraphQL data")?;
    (0..requests.len())
        .map(|index| {
            data.get(&format!("p{index}"))
                .map(|payload| payload.pull_request.clone())
                .ok_or_else(|| format!("gh output omitted created pull request {index}"))
        })
        .collect()
}

pub fn edit_prs(
    runner: &dyn CommandRunner,
    repo: &Path,
    requests: &[EditPullRequest<'_>],
) -> Result<(), String> {
    if requests.is_empty() {
        return Ok(());
    }
    let mutations = requests
        .iter()
        .enumerate()
        .map(|(index, request)| {
            format!(
                "p{index}:updatePullRequest(input:{{pullRequestId:{},title:{},body:{}}}){{pullRequest{{id number baseRefName headRefName title body isDraft url}}}}",
                graphql_string(request.id),
                graphql_string(request.title),
                graphql_string(request.body),
            )
        })
        .collect::<String>();
    let out = run_graphql(runner, repo, format!("mutation{{{mutations}}}"), &[])?;
    let response: MutationResponse = serde_json::from_str(&out)
        .map_err(|error| format!("could not parse gh output: {error}"))?;
    ensure_no_graphql_errors(&response.errors)?;
    let data = response.data.ok_or("gh output omitted GraphQL data")?;
    for index in 0..requests.len() {
        if !data.contains_key(&format!("p{index}")) {
            return Err(format!("gh output omitted edited pull request {index}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Runner {
        output: String,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl CommandRunner for Runner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            _repo: &Path,
            _input: Option<&str>,
            _env: &BTreeMap<String, String>,
        ) -> Result<String, String> {
            assert_eq!(program, "gh");
            self.calls.lock().unwrap().push(args.to_vec());
            Ok(self.output.clone())
        }
    }

    #[test]
    fn graphql_strings_escape_user_content() {
        assert_eq!(graphql_string("a\"b\nc"), "\"a\\\"b\\nc\"");
    }

    #[test]
    fn discovers_an_entire_stack_with_one_gh_process() {
        let runner = Runner {
            output: serde_json::json!({
                "data": {"repository": {
                    "id": "R_repo",
                    "p0": {"nodes": [{
                        "id": "PR_one", "number": 1,
                        "baseRefName": "fs-base/topic/1",
                        "headRefName": "fs-head/topic/1",
                        "title": "one", "body": "body",
                        "isDraft": true, "url": "https://example.invalid/1"
                    }]},
                    "p1": {"nodes": []},
                    "p2": {"nodes": []}
                }}
            })
            .to_string(),
            calls: Mutex::new(Vec::new()),
        };
        let branches = vec![
            "fs-head/topic/1".into(),
            "fs-head/topic/2".into(),
            "fs-head/topic/3".into(),
        ];
        let found = discover_prs(&runner, Path::new("."), "owner/repo", &branches).unwrap();
        assert_eq!(found.by_head.len(), 1);
        assert_eq!(runner.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn empty_mutation_batches_spawn_no_processes() {
        let runner = Runner {
            output: String::new(),
            calls: Mutex::new(Vec::new()),
        };
        assert!(
            create_prs(&runner, Path::new("."), "R_repo", &[])
                .unwrap()
                .is_empty()
        );
        edit_prs(&runner, Path::new("."), &[]).unwrap();
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn edit_requires_every_requested_alias() {
        let runner = Runner {
            output: serde_json::json!({"data": {}}).to_string(),
            calls: Mutex::new(Vec::new()),
        };
        let error = edit_prs(
            &runner,
            Path::new("."),
            &[EditPullRequest {
                id: "PR_one",
                title: "title",
                body: "body",
            }],
        )
        .unwrap_err();
        assert!(error.contains("omitted edited pull request 0"), "{error}");
    }

    #[test]
    fn edit_reports_graphql_errors_from_successful_http_responses() {
        let runner = Runner {
            output: serde_json::json!({
                "data": null,
                "errors": [{"message": "permission denied"}]
            })
            .to_string(),
            calls: Mutex::new(Vec::new()),
        };
        let error = edit_prs(
            &runner,
            Path::new("."),
            &[EditPullRequest {
                id: "PR_one",
                title: "title",
                body: "body",
            }],
        )
        .unwrap_err();
        assert!(error.contains("permission denied"), "{error}");
    }
}
