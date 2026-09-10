use std::collections::{HashMap, HashSet};

use crate::core::submit::SubmitPlan;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    pub subject: String,
    pub local_refs: Vec<String>,
    pub remote_refs: Vec<String>,
    pub tags: Vec<String>,
    pub is_head: bool,
    pub preview: bool,
    pub conflict: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub commits: HashMap<String, Commit>,
    /// Git's stable topo-order, newest first.
    pub order: Vec<String>,
    pub head: String,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovePlan {
    pub selected: String,
    pub destination: String,
    pub include_descendants: bool,
    pub base: String,
    /// Original parent of the carried substack. Substack insertion first
    /// rebases the carried commits from this base onto `destination`.
    pub source_base: String,
    /// Number of commits at the start of `commits` that belong to the carried
    /// substack. Any remaining commits are destination descendants replayed
    /// above the insertion.
    pub carried_count: usize,
    /// A local branch name when possible, otherwise a commit id.
    pub tip: String,
    pub tip_commit: String,
    /// Forkstack layer branches should follow their logical commits. Rebase
    /// them from detached HEAD so Git does not exclude the checked-out ref
    /// from `--update-refs`.
    pub detach_for_rewrite: bool,
    /// Local branch to check out at the resulting stack tip.
    pub checkout_branch: String,
    /// Local refs that should follow each logical commit during an explicit
    /// substack insertion rebase. Entries are `(commit, branch)`.
    pub ref_updates: Vec<(String, String)>,
    /// Old commit ids, in the order the rebase will replay them.
    pub commits: Vec<String>,
}

impl Graph {
    pub fn publish_preview(&self, plan: &SubmitPlan) -> Result<Self, String> {
        let mut graph = self.clone();
        let local_heads: HashSet<_> = plan
            .updates
            .iter()
            .filter(|update| update.branch.starts_with("fs-head/"))
            .map(|update| update.branch.clone())
            .collect();
        let remote_refs: HashSet<_> = plan
            .updates
            .iter()
            .map(|update| format!("{}/{}", plan.options.remote, update.branch))
            .collect();
        for commit in graph.commits.values_mut() {
            commit.local_refs.retain(|name| !local_heads.contains(name));
            commit
                .remote_refs
                .retain(|name| !remote_refs.contains(name));
        }
        for update in &plan.updates {
            let commit = graph.commits.get_mut(&update.rev).ok_or_else(|| {
                format!("publish target {} is not visible in the graph", update.rev)
            })?;
            commit.preview = true;
            let local = update.branch.clone();
            let remote = format!("{}/{}", plan.options.remote, update.branch);
            if update.branch.starts_with("fs-head/") && !commit.local_refs.contains(&local) {
                commit.local_refs.push(local);
            }
            if !commit.remote_refs.contains(&remote) {
                commit.remote_refs.push(remote);
            }
            commit.local_refs.sort();
            commit.remote_refs.sort();
        }
        graph.retain_reachable();
        Ok(graph)
    }

    fn retain_reachable(&mut self) {
        let mut pending = vec![self.head.clone()];
        pending.extend(
            self.commits
                .values()
                .filter(|commit| {
                    !commit.local_refs.is_empty()
                        || !commit.remote_refs.is_empty()
                        || !commit.tags.is_empty()
                })
                .map(|commit| commit.id.clone()),
        );
        let mut reachable = HashSet::new();
        while let Some(id) = pending.pop() {
            if !reachable.insert(id.clone()) {
                continue;
            }
            if let Some(commit) = self.commits.get(&id) {
                pending.extend(commit.parents.iter().cloned());
            }
        }
        self.commits.retain(|id, _| reachable.contains(id));
        self.order.retain(|id| reachable.contains(id));
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        let mut pending = vec![descendant.to_owned()];
        let mut seen = HashSet::new();
        while let Some(id) = pending.pop() {
            if id == ancestor {
                return true;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(commit) = self.commits.get(&id) {
                pending.extend(commit.parents.iter().cloned());
            }
        }
        false
    }

    fn first_parent(&self, id: &str) -> Result<String, String> {
        let commit = self
            .commits
            .get(id)
            .ok_or_else(|| format!("unknown commit {id}"))?;
        if commit.parents.len() != 1 {
            return Err("moves across merge commits or root commits are not supported".into());
        }
        Ok(commit.parents[0].clone())
    }

    /// Return commits after `base` through `tip`, oldest first.
    fn linear_segment(&self, base: &str, tip: &str) -> Result<Vec<String>, String> {
        let mut reverse = Vec::new();
        let mut current = tip.to_owned();
        while current != base {
            let commit = self
                .commits
                .get(&current)
                .ok_or_else(|| format!("{base} is not on the first-parent history of {tip}"))?;
            if commit.parents.len() != 1 {
                return Err("moves across merge commits are not supported".into());
            }
            reverse.push(current);
            current = commit.parents[0].clone();
        }
        reverse.reverse();
        Ok(reverse)
    }

    fn stack_tip(&self, selected: &str) -> Result<(String, String, bool), String> {
        if !self.is_ancestor(selected, &self.head) {
            return Err("the selected commit is not in the checked-out stack".into());
        }
        let Some(branch) = self.branch.as_ref() else {
            return Err("check out a local branch before rewriting it".into());
        };
        if !branch.starts_with("fs-head/") {
            return Ok((branch.clone(), self.head.clone(), false));
        }

        let mut candidates: HashMap<String, Vec<String>> = HashMap::new();
        for commit in self.commits.values() {
            if !self.is_ancestor(&self.head, &commit.id) {
                continue;
            }
            for name in &commit.local_refs {
                if name.starts_with("fs-head/") {
                    candidates
                        .entry(commit.id.clone())
                        .or_default()
                        .push(name.clone());
                }
            }
        }
        let maximal: Vec<_> = candidates
            .keys()
            .filter(|id| {
                !candidates
                    .keys()
                    .any(|other| *id != other && self.is_ancestor(id, other))
            })
            .collect();
        if maximal.is_empty() {
            Ok((branch.clone(), self.head.clone(), true))
        } else if maximal.len() == 1 {
            let id = maximal[0];
            let names = &candidates[id];
            let name = if names.contains(branch) {
                branch.clone()
            } else {
                names.iter().min().unwrap().clone()
            };
            Ok((name, (*id).clone(), true))
        } else {
            Err("the checked-out layer has multiple descendant stack tips; check out the desired stack tip".into())
        }
    }

    pub fn carried_substack(&self, selected: &str) -> Result<Vec<String>, String> {
        let (_, tip_commit, _) = self.stack_tip(selected)?;
        let parent = self.first_parent(selected)?;
        self.linear_segment(&parent, &tip_commit)
    }

    fn descendant_stack_tip(&self, destination: &str) -> Result<(String, String), String> {
        let mut candidates: HashMap<String, Vec<String>> = HashMap::new();
        for commit in self.commits.values() {
            if !self.is_ancestor(destination, &commit.id) {
                continue;
            }
            for name in &commit.local_refs {
                if name.starts_with("fs-head/") {
                    candidates
                        .entry(commit.id.clone())
                        .or_default()
                        .push(name.clone());
                }
            }
        }
        let maximal: Vec<_> = candidates
            .keys()
            .filter(|id| {
                !candidates
                    .keys()
                    .any(|other| *id != other && self.is_ancestor(id, other))
            })
            .collect();
        if maximal.is_empty() {
            Ok((destination.to_owned(), destination.to_owned()))
        } else if maximal.len() == 1 {
            let id = maximal[0];
            let name = candidates[id].iter().min().unwrap().clone();
            Ok((name, (*id).clone()))
        } else {
            Err("the destination has multiple descendant stack tips; check out or choose an unambiguous destination".into())
        }
    }

    pub fn plan_move(
        &self,
        selected: &str,
        destination: &str,
        include_descendants: bool,
    ) -> Result<MovePlan, String> {
        if selected == destination {
            return Err("a commit cannot be dropped onto itself".into());
        }
        let (tip, tip_commit, detach_for_rewrite) = self.stack_tip(selected)?;
        let parent = self.first_parent(selected)?;

        if include_descendants {
            if destination == parent {
                return Err("that move would not change the stack".into());
            }
            let carried = self.linear_segment(&parent, &tip_commit)?;
            if self.is_ancestor(selected, destination) {
                return Err("the destination is inside the selected substack".into());
            }
            let (destination_tip, destination_tip_commit) =
                self.descendant_stack_tip(destination)?;
            let destination_descendants =
                self.linear_segment(destination, &destination_tip_commit)?;
            if carried
                .iter()
                .any(|id| destination_descendants.contains(id))
            {
                return Err(
                    "the destination descendant chain overlaps the selected substack".into(),
                );
            }
            let carried_count = carried.len();
            let mut commits = carried;
            commits.extend(destination_descendants);
            let resulting_tip = if commits.len() == carried_count {
                tip.clone()
            } else {
                destination_tip
            };
            let resulting_tip_commit = commits.last().cloned().ok_or("move has no commits")?;
            let checkout_branch =
                self.checkout_branch(&commits, &resulting_tip, detach_for_rewrite)?;
            let ref_updates = self.ref_updates(&commits);
            return Ok(MovePlan {
                selected: selected.into(),
                destination: destination.into(),
                include_descendants: true,
                base: destination.into(),
                source_base: parent,
                carried_count,
                tip: resulting_tip,
                tip_commit: resulting_tip_commit,
                detach_for_rewrite,
                checkout_branch,
                ref_updates,
                commits,
            });
        }

        if !self.is_ancestor(destination, &tip_commit) {
            return Err("a single commit can only move within the checked-out stack; pick up its substack to move it elsewhere".into());
        }
        let moving_later = self.is_ancestor(selected, destination);
        let base = if moving_later {
            parent.clone()
        } else {
            destination.into()
        };
        let original = self.linear_segment(&base, &tip_commit)?;
        let mut commits = original.clone();
        commits.retain(|id| id != selected);
        let insertion = if moving_later {
            commits
                .iter()
                .position(|id| id == destination)
                .ok_or("destination is not in the linear stack")?
                + 1
        } else {
            0
        };
        commits.insert(insertion, selected.into());
        if commits == original {
            return Err("that move would not change the stack".into());
        }
        let checkout_branch = self.checkout_branch(&commits, &tip, detach_for_rewrite)?;
        let ref_updates = self.ref_updates(&commits);
        Ok(MovePlan {
            selected: selected.into(),
            destination: destination.into(),
            include_descendants: false,
            base,
            source_base: parent,
            carried_count: 1,
            tip,
            tip_commit,
            detach_for_rewrite,
            checkout_branch,
            ref_updates,
            commits,
        })
    }

    fn ref_updates(&self, commits: &[String]) -> Vec<(String, String)> {
        let mut updates = Vec::new();
        for id in commits {
            if let Some(commit) = self.commits.get(id) {
                updates.extend(
                    commit
                        .local_refs
                        .iter()
                        .map(|branch| (id.clone(), branch.clone())),
                );
            }
        }
        updates.sort();
        updates
    }

    fn checkout_branch(
        &self,
        commits: &[String],
        tip: &str,
        detach_for_rewrite: bool,
    ) -> Result<String, String> {
        if !detach_for_rewrite {
            return Ok(tip.to_owned());
        }
        let final_id = commits.last().ok_or("move has no commits")?;
        let final_commit = self
            .commits
            .get(final_id)
            .ok_or_else(|| format!("unknown commit {final_id}"))?;
        let mut branches: Vec<_> = final_commit
            .local_refs
            .iter()
            .filter(|name| name.starts_with("fs-head/"))
            .cloned()
            .collect();
        branches.sort();
        if branches.iter().any(|name| name == tip) {
            Ok(tip.to_owned())
        } else {
            branches.into_iter().next().ok_or_else(|| {
                "the resulting Forkstack tip has no fs-head branch; pick up its substack instead".into()
            })
        }
    }

    /// Construct Sapling-style virtual rewritten commits. Real commits and
    /// remote refs stay put; local refs and HEAD follow their virtual copies.
    pub fn preview(&self, plan: &MovePlan) -> Result<Self, String> {
        let mut graph = self.clone();
        let rewritten: HashSet<_> = plan.commits.iter().cloned().collect();
        let mut virtual_ids = HashMap::new();
        for old in &plan.commits {
            virtual_ids.insert(old.clone(), format!("preview:{old}"));
        }

        let mut parent = if plan.include_descendants {
            plan.destination.clone()
        } else {
            plan.base.clone()
        };
        for old in &plan.commits {
            let mut commit = self
                .commits
                .get(old)
                .ok_or_else(|| format!("unknown commit {old}"))?
                .clone();
            let virtual_id = virtual_ids[old].clone();
            commit.id = virtual_id.clone();
            commit.parents = vec![parent.clone()];
            commit.preview = true;
            commit.remote_refs.clear();
            commit.tags.clear();
            commit.is_head = false;
            graph.commits.insert(virtual_id.clone(), commit);
            parent = virtual_id;
        }

        for old in &rewritten {
            if let Some(commit) = graph.commits.get_mut(old) {
                commit.local_refs.clear();
                commit.is_head = false;
            }
        }
        let old_tip_virtual = virtual_ids
            .get(&plan.tip_commit)
            .cloned()
            .ok_or("tip was not rewritten")?;
        let final_old = plan.commits.last().ok_or("move has no commits")?;
        let final_virtual = virtual_ids
            .get(final_old)
            .cloned()
            .ok_or("final commit was not rewritten")?;
        // Ordinary checked-out branches follow the resulting stack tip. For
        // Forkstack layers, Git runs detached and every fs-head ref follows
        // the logical commit it decorated before the rewrite.
        if !plan.detach_for_rewrite {
            if let Some(commit) = graph.commits.get_mut(&old_tip_virtual) {
                commit.local_refs.retain(|name| name != &plan.tip);
            }
        }
        let final_commit = graph.commits.get_mut(&final_virtual).unwrap();
        if !plan.detach_for_rewrite && !final_commit.local_refs.contains(&plan.tip) {
            final_commit.local_refs.push(plan.tip.clone());
            final_commit.local_refs.sort();
        }
        final_commit.is_head = true;
        graph.head = final_virtual;
        graph.branch = Some(plan.checkout_branch.clone());
        // Virtual first-parent chains are shown first; original remote history
        // remains available after them.
        let mut order: Vec<_> = plan
            .commits
            .iter()
            .rev()
            .map(|id| virtual_ids[id].clone())
            .collect();
        order.extend(self.order.iter().cloned());
        let mut seen = HashSet::new();
        graph.order = order
            .into_iter()
            .filter(|id| seen.insert(id.clone()))
            .collect();
        Ok(graph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> Graph {
        let ids = ["a", "b", "c", "d"];
        let mut commits = HashMap::new();
        for (index, id) in ids.iter().enumerate() {
            commits.insert(
                (*id).into(),
                Commit {
                    id: (*id).into(),
                    parents: index
                        .checked_sub(1)
                        .map(|i| vec![ids[i].into()])
                        .unwrap_or_default(),
                    subject: (*id).into(),
                    local_refs: if *id == "d" {
                        vec!["main".into()]
                    } else {
                        Vec::new()
                    },
                    remote_refs: if *id == "d" {
                        vec!["origin/main".into()]
                    } else {
                        Vec::new()
                    },
                    is_head: *id == "d",
                    ..Commit::default()
                },
            );
        }
        Graph {
            commits,
            order: ids.iter().rev().map(|s| (*s).into()).collect(),
            head: "d".into(),
            branch: Some("main".into()),
        }
    }

    #[test]
    fn plans_single_commit_and_substack_moves() {
        let mut graph = graph();
        let single = graph.plan_move("b", "c", false).unwrap();
        assert_eq!(
            single.commits,
            vec!["c".to_owned(), "b".to_owned(), "d".to_owned()]
        );
        graph.commits.insert(
            "x".into(),
            Commit {
                id: "x".into(),
                parents: vec!["a".into()],
                subject: "other stack".into(),
                ..Commit::default()
            },
        );
        let substack = graph.plan_move("c", "x", true).unwrap();
        assert!(substack.include_descendants);
        assert_eq!(substack.commits, vec!["c".to_owned(), "d".to_owned()]);
        assert_eq!(substack.destination, "x");
    }

    #[test]
    fn intermediate_forkstack_branch_uses_unique_descendant_tip() {
        let mut graph = graph();
        graph.commits.get_mut("b").unwrap().local_refs = vec!["fs-head/test/1".into()];
        graph.commits.get_mut("c").unwrap().local_refs = vec!["fs-head/test/2".into()];
        graph.commits.get_mut("d").unwrap().local_refs = vec!["fs-head/test/3".into()];
        graph.commits.get_mut("d").unwrap().is_head = false;
        graph.commits.get_mut("c").unwrap().is_head = true;
        graph.head = "c".into();
        graph.branch = Some("fs-head/test/2".into());

        let plan = graph.plan_move("b", "c", false).unwrap();
        assert_eq!(plan.tip, "fs-head/test/3");
        assert_eq!(plan.tip_commit, "d");
        assert_eq!(
            plan.commits,
            vec!["c".to_owned(), "b".to_owned(), "d".to_owned()]
        );
    }

    #[test]
    fn preview_moves_local_refs_but_preserves_remote_refs() {
        let graph = graph();
        let plan = graph.plan_move("b", "c", false).unwrap();
        let preview = graph.preview(&plan).unwrap();
        assert!(
            preview.commits["d"]
                .remote_refs
                .contains(&"origin/main".into())
        );
        assert!(preview.commits["d"].local_refs.is_empty());
        assert!(
            preview.commits["preview:d"]
                .local_refs
                .contains(&"main".into())
        );
        assert_eq!(preview.head, "preview:d");
        assert_eq!(
            preview.commits["preview:b"].parents,
            vec!["preview:c".to_owned()]
        );
    }

    #[test]
    fn publish_preview_decorates_head_and_base_targets() {
        let mut graph = graph();
        graph
            .commits
            .get_mut("c")
            .unwrap()
            .local_refs
            .push("fs-head/test/1".into());
        graph
            .commits
            .get_mut("c")
            .unwrap()
            .remote_refs
            .push("origin/fs-head/test/1".into());
        graph
            .commits
            .get_mut("d")
            .unwrap()
            .remote_refs
            .push("origin/fs-base/test/1".into());
        let plan = SubmitPlan {
            options: crate::core::submit::SubmitOptions {
                remote: "origin".into(),
                ..crate::core::submit::SubmitOptions::default()
            },
            fork: "example/repo".into(),
            base_ref: "origin/main".into(),
            commits: Vec::new(),
            updates: vec![
                crate::core::submit::RefUpdate {
                    branch: "fs-base/test/1".into(),
                    rev: "a".into(),
                },
                crate::core::submit::RefUpdate {
                    branch: "fs-head/test/1".into(),
                    rev: "b".into(),
                },
            ],
        };
        let preview = graph.publish_preview(&plan).unwrap();
        assert!(
            preview.commits["a"]
                .remote_refs
                .contains(&"origin/fs-base/test/1".into())
        );
        assert!(
            preview.commits["b"]
                .remote_refs
                .contains(&"origin/fs-head/test/1".into())
        );
        assert!(
            preview.commits["b"]
                .local_refs
                .contains(&"fs-head/test/1".into())
        );
        assert!(preview.commits["a"].preview && preview.commits["b"].preview);
        assert!(
            !preview.commits["c"]
                .local_refs
                .contains(&"fs-head/test/1".into())
        );
        assert!(
            !preview.commits["c"]
                .remote_refs
                .contains(&"origin/fs-head/test/1".into())
        );
        assert!(
            !preview.commits["d"]
                .remote_refs
                .contains(&"origin/fs-base/test/1".into())
        );
    }

    #[test]
    fn publish_preview_omits_commits_abandoned_by_moved_remote_ref() {
        let mut graph = graph();
        graph.commits.insert(
            "remote-old".into(),
            Commit {
                id: "remote-old".into(),
                parents: vec!["a".into()],
                subject: "old remote commit".into(),
                remote_refs: vec!["origin/fs-base/test/1".into()],
                ..Commit::default()
            },
        );
        graph.order.insert(0, "remote-old".into());
        let plan = SubmitPlan {
            options: crate::core::submit::SubmitOptions {
                remote: "origin".into(),
                ..crate::core::submit::SubmitOptions::default()
            },
            fork: "example/repo".into(),
            base_ref: "origin/main".into(),
            commits: Vec::new(),
            updates: vec![crate::core::submit::RefUpdate {
                branch: "fs-base/test/1".into(),
                rev: "b".into(),
            }],
        };

        let preview = graph.publish_preview(&plan).unwrap();

        assert!(!preview.commits.contains_key("remote-old"));
        assert!(!preview.order.contains(&"remote-old".to_owned()));
        assert!(preview.commits.contains_key("a"));
        assert!(preview.order.contains(&"a".to_owned()));
    }

    #[test]
    fn preview_places_tip_branch_on_last_replayed_commit() {
        let mut graph = graph();
        graph
            .commits
            .get_mut("d")
            .unwrap()
            .local_refs
            .push("marker".into());
        let plan = graph.plan_move("b", "d", false).unwrap();
        assert_eq!(
            plan.commits,
            vec!["c".to_owned(), "d".to_owned(), "b".to_owned()]
        );

        let preview = graph.preview(&plan).unwrap();

        assert_eq!(preview.head, "preview:b");
        assert!(preview.commits["preview:b"].is_head);
        assert!(
            preview.commits["preview:b"]
                .local_refs
                .contains(&"main".into())
        );
        assert!(
            preview.commits["preview:d"]
                .local_refs
                .contains(&"marker".into())
        );
        assert!(
            !preview.commits["preview:d"]
                .local_refs
                .contains(&"main".into())
        );
        assert!(
            preview.commits["d"]
                .remote_refs
                .contains(&"origin/main".into())
        );
    }

    #[test]
    fn forkstack_preview_keeps_layer_refs_with_logical_commits() {
        let mut graph = graph();
        graph.commits.get_mut("b").unwrap().local_refs = vec!["fs-head/test/1".into()];
        graph.commits.get_mut("c").unwrap().local_refs = vec!["fs-head/test/2".into()];
        graph.commits.get_mut("d").unwrap().local_refs = vec!["fs-head/test/3".into()];
        graph.branch = Some("fs-head/test/3".into());

        let plan = graph.plan_move("c", "d", false).unwrap();
        assert_eq!(plan.commits, vec!["d".to_owned(), "c".to_owned()]);
        assert!(plan.detach_for_rewrite);
        assert_eq!(plan.checkout_branch, "fs-head/test/2");

        let preview = graph.preview(&plan).unwrap();
        assert!(
            preview.commits["preview:c"]
                .local_refs
                .contains(&"fs-head/test/2".into())
        );
        assert!(
            preview.commits["preview:d"]
                .local_refs
                .contains(&"fs-head/test/3".into())
        );
        assert_eq!(preview.head, "preview:c");
        assert_eq!(preview.branch.as_deref(), Some("fs-head/test/2"));
    }

    fn insertion_graph() -> Graph {
        let entries = [
            ("root", None, None),
            ("alpha2", Some("root"), Some("fs-head/alpha/2")),
            ("alpha3", Some("alpha2"), Some("fs-head/alpha/3")),
            ("gamma1", Some("alpha3"), Some("fs-head/gamma/1")),
            ("gamma2", Some("gamma1"), Some("fs-head/gamma/2")),
            ("gamma3", Some("gamma2"), Some("fs-head/gamma/3")),
            ("beta2", Some("root"), Some("fs-head/beta/2")),
            ("beta3", Some("beta2"), Some("fs-head/beta/3")),
        ];
        let mut commits = HashMap::new();
        for (id, parent, branch) in entries {
            commits.insert(
                id.into(),
                Commit {
                    id: id.into(),
                    parents: parent.into_iter().map(str::to_owned).collect(),
                    subject: id.into(),
                    local_refs: branch.into_iter().map(str::to_owned).collect(),
                    is_head: id == "beta3",
                    ..Commit::default()
                },
            );
        }
        Graph {
            commits,
            order: entries
                .iter()
                .rev()
                .map(|(id, _, _)| (*id).into())
                .collect(),
            head: "beta3".into(),
            branch: Some("fs-head/beta/3".into()),
        }
    }

    #[test]
    fn substack_insertion_replays_destination_descendants_above_carried_commits() {
        let graph = insertion_graph();
        assert_eq!(graph.carried_substack("beta2").unwrap(), ["beta2", "beta3"]);
        let plan = graph.plan_move("beta3", "alpha2", true).unwrap();

        assert_eq!(plan.base, "alpha2");
        assert_eq!(plan.source_base, "beta2");
        assert_eq!(plan.carried_count, 1);
        assert_eq!(
            plan.commits,
            ["beta3", "alpha3", "gamma1", "gamma2", "gamma3"]
        );
        assert_eq!(plan.checkout_branch, "fs-head/gamma/3");

        let preview = graph.preview(&plan).unwrap();
        assert_eq!(preview.commits["preview:beta3"].parents, ["alpha2"]);
        assert_eq!(preview.commits["preview:alpha3"].parents, ["preview:beta3"]);
        assert_eq!(
            preview.commits["preview:gamma1"].parents,
            ["preview:alpha3"]
        );
        assert_eq!(preview.head, "preview:gamma3");
        assert_eq!(preview.branch.as_deref(), Some("fs-head/gamma/3"));
    }

    #[test]
    fn substack_insertion_rejects_ambiguous_destination_descendants() {
        let mut graph = insertion_graph();
        graph.commits.insert(
            "other-tip".into(),
            Commit {
                id: "other-tip".into(),
                parents: vec!["alpha2".into()],
                subject: "other tip".into(),
                local_refs: vec!["fs-head/other/1".into()],
                ..Commit::default()
            },
        );

        let error = graph.plan_move("beta3", "alpha2", true).unwrap_err();
        assert!(error.contains("multiple descendant stack tips"));
    }
}
