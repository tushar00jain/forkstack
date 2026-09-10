pub mod git;
pub mod github;
pub mod log;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

pub trait CommandRunner: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[String],
        repo: &Path,
        input: Option<&str>,
        env: &BTreeMap<String, String>,
    ) -> Result<String, String>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        repo: &Path,
        input: Option<&str>,
        env: &BTreeMap<String, String>,
    ) -> Result<String, String> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(repo)
            .envs(env)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not run {program}: {error}"))?;
        if let Some(input) = input {
            child
                .stdin
                .take()
                .ok_or("could not open command stdin")?
                .write_all(input.as_bytes())
                .map_err(|error| error.to_string())?;
        }
        let result = child
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        if result.status.success() {
            Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
        } else {
            let stderr = String::from_utf8_lossy(&result.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&result.stdout).trim().to_owned();
            Err(if stderr.is_empty() { stdout } else { stderr })
        }
    }
}
