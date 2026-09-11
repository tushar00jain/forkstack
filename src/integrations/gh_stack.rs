use std::collections::BTreeMap;
use std::path::Path;

use super::CommandRunner;

pub fn submit(runner: &dyn CommandRunner, repo: &Path, github_repo: &str) -> Result<(), String> {
    let args = ["stack", "submit", "--remote", "upstream", "--auto"]
        .map(str::to_owned)
        .to_vec();
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
    fn submit_runs_the_confirmed_non_interactive_upstream_command() {
        let runner = Runner::default();

        submit(&runner, Path::new("repo"), "meta-pytorch/torchstore").unwrap();

        assert_eq!(
            *runner.calls.lock().unwrap(),
            [(
                "gh".into(),
                ["stack", "submit", "--remote", "upstream", "--auto"]
                    .map(str::to_owned)
                    .to_vec(),
                BTreeMap::from([("GH_REPO".into(), "meta-pytorch/torchstore".into())]),
            )]
        );
    }
}
