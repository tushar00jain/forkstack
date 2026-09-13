use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use forkstack::ui::workspace::discover;
use git2::{Repository, Signature};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "forkstack-workspace-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn repository(path: &Path) -> Repository {
    let repo = Repository::init(path).unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    let signature = Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "initial",
        &repo.find_tree(tree_id).unwrap(),
        &[],
    )
    .unwrap();
    repo
}

#[test]
fn discovers_only_root_and_immediate_children_in_stable_order() {
    let fixture = Fixture::new();
    repository(&fixture.0);
    repository(&fixture.0.join("zeta"));
    Repository::init(fixture.0.join("alpha")).unwrap(); // Unborn repositories are discoverable.
    assert!(
        forkstack::ui::git::load_graph(&fixture.0.join("alpha"))
            .unwrap()
            .order
            .is_empty()
    );
    repository(&fixture.0.join("group/deeper"));
    fs::create_dir_all(fixture.0.join("ordinary")).unwrap();
    fs::create_dir_all(fixture.0.join("invalid/.git")).unwrap();
    Repository::init_bare(fixture.0.join("bare.git")).unwrap();
    let workspace = discover(&fixture.0).unwrap();
    assert_eq!(workspace.initial, Some(fixture.0.clone()));
    assert_eq!(
        workspace.repositories,
        vec![
            fixture.0.clone(),
            fixture.0.join("alpha"),
            fixture.0.join("zeta")
        ]
    );
}

#[test]
fn parent_folder_does_not_activate_a_repository_and_rescan_finds_changes() {
    let fixture = Fixture::new();
    assert!(discover(&fixture.0).unwrap().repositories.is_empty());
    repository(&fixture.0.join("one"));
    let workspace = discover(&fixture.0).unwrap();
    assert!(workspace.initial.is_none());
    assert_eq!(workspace.repositories, vec![fixture.0.join("one")]);
    fs::rename(fixture.0.join("one"), fixture.0.join("renamed")).unwrap();
    assert_eq!(
        discover(&fixture.0).unwrap().repositories,
        vec![fixture.0.join("renamed")]
    );
    fs::remove_dir_all(fixture.0.join("renamed")).unwrap();
    assert!(discover(&fixture.0).unwrap().repositories.is_empty());
}

#[test]
fn launching_inside_a_repository_keeps_the_enclosing_repository() {
    let fixture = Fixture::new();
    repository(&fixture.0.join("repo"));
    fs::create_dir_all(fixture.0.join("repo/src/nested")).unwrap();
    let workspace = discover(&fixture.0.join("repo/src/nested")).unwrap();
    assert_eq!(workspace.initial, Some(fixture.0.join("repo")));
    assert_eq!(workspace.repositories, vec![fixture.0.join("repo")]);
}

#[test]
fn worktrees_with_git_files_remain_distinct_from_the_main_checkout() {
    let fixture = Fixture::new();
    let repo = repository(&fixture.0.join("main"));
    repo.worktree("linked", &fixture.0.join("linked"), None)
        .unwrap();
    assert!(fixture.0.join("linked/.git").is_file());
    assert_eq!(
        discover(&fixture.0).unwrap().repositories,
        vec![fixture.0.join("linked"), fixture.0.join("main")]
    );
    assert_eq!(
        discover(&fixture.0.join("linked")).unwrap().initial,
        Some(fixture.0.join("linked"))
    );
}

#[cfg(unix)]
#[test]
fn child_symlinks_are_skipped_but_an_explicit_symlink_root_is_canonicalized() {
    let fixture = Fixture::new();
    repository(&fixture.0.join("repo"));
    std::os::unix::fs::symlink(fixture.0.join("repo"), fixture.0.join("alias")).unwrap();
    assert_eq!(
        discover(&fixture.0).unwrap().repositories,
        vec![fixture.0.join("repo")]
    );
    assert_eq!(
        discover(&fixture.0.join("alias")).unwrap().initial,
        Some(fixture.0.join("repo"))
    );
    assert!(discover(&fixture.0.join("missing")).is_err());
}
