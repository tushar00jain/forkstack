use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use git2::{Repository, RepositoryOpenFlags};

/// Repository discovery never reads history. Worktrees are identified by their
/// working directory, not their shared Git directory.
pub struct Workspace {
    pub root: PathBuf,
    pub repositories: Vec<PathBuf>,
    pub initial: Option<PathBuf>,
}

fn workdir(repository: Repository) -> Option<PathBuf> {
    repository.workdir()?.canonicalize().ok()
}

fn exact_repository(path: &Path) -> Option<PathBuf> {
    if !path.join(".git").exists() {
        return None;
    }
    Repository::open_ext(path, RepositoryOpenFlags::NO_SEARCH, &[] as &[&Path])
        .ok()
        .and_then(workdir)
        .filter(|root| path.canonicalize().ok().as_ref() == Some(root))
}

pub fn discover(path: &Path) -> Result<Workspace, String> {
    let root = path.canonicalize().map_err(|error| error.to_string())?;
    let entries = std::fs::read_dir(&root)
        .map_err(|error| format!("cannot read {}: {error}", root.display()))?;
    let initial = exact_repository(&root);
    let mut repositories = BTreeSet::new();
    repositories.extend(initial.clone());
    for entry in entries.flatten() {
        // Do not follow directory symlinks or scan grandchildren.
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            repositories.extend(exact_repository(&entry.path()));
        }
    }
    // Retain launching from a subdirectory of a repository. Prefer repositories
    // actually found in the requested folder over an unrelated enclosing root.
    let initial = if repositories.is_empty() {
        let enclosing = Repository::discover(&root).ok().and_then(workdir);
        repositories.extend(enclosing.clone());
        enclosing
    } else {
        initial
    };
    Ok(Workspace {
        root,
        repositories: repositories.into_iter().collect(),
        initial,
    })
}
