use std::collections::{HashMap, HashSet};

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
    /// A local branch name when possible, otherwise a commit id.
    pub tip: String,
    pub tip_commit: String,
    /// Old commit ids, in the order the rebase will replay them.
    pub commits: Vec<String>,
}

impl Graph {
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
        let commit = self.commits.get(id).ok_or_else(|| format!("unknown commit {id}"))?;
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
            let commit = self.commits.get(&current).ok_or_else(|| {
                format!("{base} is not on the first-parent history of {tip}")
            })?;
            if commit.parents.len() != 1 {
                return Err("moves across merge commits are not supported".into());
            }
            reverse.push(current);
            current = commit.parents[0].clone();
        }
        reverse.reverse();
        Ok(reverse)
    }

    fn stack_tip(&self, selected: &str) -> Result<(String, String), String> {
        if !self.is_ancestor(selected, &self.head) {
            return Err("the selected commit is not in the checked-out stack".into());
        }
        let Some(branch) = self.branch.as_ref() else {
            return Err("check out a local branch before rewriting it".into());
        };
        if !branch.starts_with("fs-head/") {
            return Ok((branch.clone(), self.head.clone()));
        }

        let mut candidates: HashMap<String, Vec<String>> = HashMap::new();
        for commit in self.commits.values() {
            if !self.is_ancestor(&self.head, &commit.id) {
                continue;
            }
            for name in &commit.local_refs {
                if name.starts_with("fs-head/") {
                    candidates.entry(commit.id.clone()).or_default().push(name.clone());
                }
            }
        }
        let maximal: Vec<_> = candidates
            .keys()
            .filter(|id| {
                !candidates.keys().any(|other| *id != other && self.is_ancestor(id, other))
            })
            .collect();
        if maximal.is_empty() {
            Ok((branch.clone(), self.head.clone()))
        } else if maximal.len() == 1 {
            let id = maximal[0];
            let names = &candidates[id];
            let name = if names.contains(branch) {
                branch.clone()
            } else {
                names.iter().min().unwrap().clone()
            };
            Ok((name, (*id).clone()))
        } else {
            Err("the checked-out layer has multiple descendant stack tips; check out the desired stack tip".into())
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
        let (tip, tip_commit) = self.stack_tip(selected)?;
        let parent = self.first_parent(selected)?;

        if include_descendants {
            if destination == parent {
                return Err("that move would not change the stack".into());
            }
            let commits = self.linear_segment(&parent, &tip_commit)?;
            if self.is_ancestor(selected, destination) {
                return Err("the destination is inside the selected substack".into());
            }
            return Ok(MovePlan {
                selected: selected.into(),
                destination: destination.into(),
                include_descendants: true,
                base: parent,
                tip,
                tip_commit,
                commits,
            });
        }

        if !self.is_ancestor(destination, &tip_commit) {
            return Err("a single commit can only move within the checked-out stack; pick up its substack to move it elsewhere".into());
        }
        let moving_later = self.is_ancestor(selected, destination);
        let base = if moving_later { parent } else { destination.into() };
        let original = self.linear_segment(&base, &tip_commit)?;
        let mut commits = original.clone();
        commits.retain(|id| id != selected);
        let insertion = if moving_later {
            commits.iter().position(|id| id == destination).ok_or("destination is not in the linear stack")? + 1
        } else {
            0
        };
        commits.insert(insertion, selected.into());
        if commits == original {
            return Err("that move would not change the stack".into());
        }
        Ok(MovePlan {
            selected: selected.into(),
            destination: destination.into(),
            include_descendants: false,
            base,
            tip,
            tip_commit,
            commits,
        })
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
            let mut commit = self.commits.get(old).ok_or_else(|| format!("unknown commit {old}"))?.clone();
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
        let old_tip_virtual = virtual_ids.get(&plan.tip_commit).cloned().ok_or("tip was not rewritten")?;
        let final_old = plan.commits.last().ok_or("move has no commits")?;
        let final_virtual = virtual_ids.get(final_old).cloned().ok_or("final commit was not rewritten")?;
        // `--update-refs` moves every local ref with its corresponding commit,
        // except the checked-out tip branch: after a reorder that branch and
        // HEAD point at the last replayed commit.
        if let Some(commit) = graph.commits.get_mut(&old_tip_virtual) {
            commit.local_refs.retain(|name| name != &plan.tip);
        }
        let final_commit = graph.commits.get_mut(&final_virtual).unwrap();
        if !final_commit.local_refs.contains(&plan.tip) {
            final_commit.local_refs.push(plan.tip.clone());
            final_commit.local_refs.sort();
        }
        final_commit.is_head = true;
        graph.head = final_virtual;
        graph.branch = Some(plan.tip.clone());
        // Virtual first-parent chains are shown first; original remote history
        // remains available after them.
        let mut order: Vec<_> = plan.commits.iter().rev().map(|id| virtual_ids[id].clone()).collect();
        order.extend(self.order.iter().cloned());
        let mut seen = HashSet::new();
        graph.order = order.into_iter().filter(|id| seen.insert(id.clone())).collect();
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
            commits.insert((*id).into(), Commit {
                id: (*id).into(),
                parents: index.checked_sub(1).map(|i| vec![ids[i].into()]).unwrap_or_default(),
                subject: (*id).into(),
                local_refs: if *id == "d" { vec!["main".into()] } else { Vec::new() },
                remote_refs: if *id == "d" { vec!["origin/main".into()] } else { Vec::new() },
                is_head: *id == "d",
                ..Commit::default()
            });
        }
        Graph { commits, order: ids.iter().rev().map(|s| (*s).into()).collect(), head: "d".into(), branch: Some("main".into()) }
    }

    #[test]
    fn plans_single_commit_and_substack_moves() {
        let mut graph = graph();
        let single = graph.plan_move("b", "c", false).unwrap();
        assert_eq!(single.commits, vec!["c".to_owned(), "b".to_owned(), "d".to_owned()]);
        graph.commits.insert("x".into(), Commit {
            id: "x".into(),
            parents: vec!["a".into()],
            subject: "other stack".into(),
            ..Commit::default()
        });
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
        assert_eq!(plan.commits, vec!["c".to_owned(), "b".to_owned(), "d".to_owned()]);
    }

    #[test]
    fn preview_moves_local_refs_but_preserves_remote_refs() {
        let graph = graph();
        let plan = graph.plan_move("b", "c", false).unwrap();
        let preview = graph.preview(&plan).unwrap();
        assert!(preview.commits["d"].remote_refs.contains(&"origin/main".into()));
        assert!(preview.commits["d"].local_refs.is_empty());
        assert!(preview.commits["preview:d"].local_refs.contains(&"main".into()));
        assert_eq!(preview.head, "preview:d");
        assert_eq!(preview.commits["preview:b"].parents, vec!["preview:c".to_owned()]);
    }

    #[test]
    fn preview_places_tip_branch_on_last_replayed_commit() {
        let mut graph = graph();
        graph.commits.get_mut("d").unwrap().local_refs.push("marker".into());
        let plan = graph.plan_move("b", "d", false).unwrap();
        assert_eq!(plan.commits, vec!["c".to_owned(), "d".to_owned(), "b".to_owned()]);

        let preview = graph.preview(&plan).unwrap();

        assert_eq!(preview.head, "preview:b");
        assert!(preview.commits["preview:b"].is_head);
        assert!(preview.commits["preview:b"].local_refs.contains(&"main".into()));
        assert!(preview.commits["preview:d"].local_refs.contains(&"marker".into()));
        assert!(!preview.commits["preview:d"].local_refs.contains(&"main".into()));
        assert!(preview.commits["d"].remote_refs.contains(&"origin/main".into()));
    }
}
