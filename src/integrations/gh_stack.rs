use std::collections::BTreeMap;
use std::path::Path;

use super::CommandRunner;

pub fn link(
    runner: &dyn CommandRunner,
    repo: &Path,
    github_repo: &str,
    remote: &str,
    base: &str,
    branches: &[String],
) -> Result<(), String> {
    let mut args = ["stack", "link", "--remote", remote, "--base", base]
        .map(str::to_owned)
        .to_vec();
    args.extend(branches.iter().cloned());
    let env = BTreeMap::from([("GH_REPO".into(), github_repo.to_owned())]);
    runner.run("gh", &args, repo, None, &env).map(|_| ())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Runner {
        calls: Mutex<Vec<(String, Vec<String>, BTreeMap<String, String>)>>,
    }

    impl CommandRunner for Runner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            _repo: &Path,
            input: Option<&str>,
            env: &BTreeMap<String, String>,
        ) -> Result<String, String> {
            assert!(input.is_none());
            self.calls
                .lock()
                .unwrap()
                .push((program.into(), args.to_vec(), env.clone()));
            Ok(String::new())
        }
    }

    #[test]
    fn link_runs_for_the_published_origin_branches() {
        let runner = Runner::default();

        link(
            &runner,
            Path::new("repo"),
            "owner/repo",
            "origin",
            "main",
            &["fs-head/topic/1".into(), "fs-head/topic/2".into()],
        )
        .unwrap();

        assert_eq!(
            *runner.calls.lock().unwrap(),
            [(
                "gh".into(),
                [
                    "stack",
                    "link",
                    "--remote",
                    "origin",
                    "--base",
                    "main",
                    "fs-head/topic/1",
                    "fs-head/topic/2",
                ]
                .map(str::to_owned)
                .to_vec(),
                BTreeMap::from([("GH_REPO".into(), "owner/repo".into())]),
            )]
        );
    }
}
