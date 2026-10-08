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
pub struct DeleteBranchPlan {
    pub branch: String,
    pub branches: Vec<String>,
    pub expected_branches: Vec<(String, String)>,
    pub current_branch: Option<String>,
    pub expected_head: String,
    pub checkout_branch: String,
    pub expected_checkout: String,
    pub remote_branches: Vec<(String, String)>,
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
    /// selection. Reorder plans may append destination descendants.
    pub carried_count: usize,
    pub mode: MoveMode,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayPlan {
    pub onto: String,
    pub upstream: String,
    pub branch: String,
    pub ref_updates: Vec<(String, String)>,
    pub commits: Vec<String>,
}

impl MovePlan {
    pub fn replay_plan(&self) -> ReplayPlan {
        ReplayPlan {
            onto: self.destination.clone(),
            upstream: if self.commits.len() == self.carried_count {
                self.source_base.clone()
            } else {
                self.destination.clone()
            },
            branch: self.checkout_branch.clone(),
            ref_updates: self.ref_updates.clone(),
            commits: self.commits.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MoveMode {
    #[default]
    Direct,
    Reorder,
}

impl Graph {
    pub fn plan_delete_current_branch(&self, base: &str) -> Result<DeleteBranchPlan, String> {
        self.plan_delete_current_branches(base, false)
    }

    pub fn plan_delete_current_branches(
        &self,
        base: &str,
        lower_stack: bool,
    ) -> Result<DeleteBranchPlan, String> {
        let branch = self
            .branch
            .as_deref()
            .ok_or("cannot delete from a detached HEAD")?;
        branch
            .strip_prefix("fs-head/")
            .filter(|name| !name.is_empty())
            .ok_or("the checked-out branch is not a ForkStack fs-head branch")?;
        self.plan_delete_branches(branch, base, lower_stack)
    }

    pub fn plan_delete_branches(
        &self,
        branch: &str,
        base: &str,
        lower_stack: bool,
    ) -> Result<DeleteBranchPlan, String> {
        branch
            .strip_prefix("fs-head/")
            .filter(|name| !name.is_empty())
            .ok_or("the branch is not a ForkStack fs-head branch")?;
        let source = self
            .commits
            .values()
            .find(|commit| commit.local_refs.iter().any(|name| name == branch))
            .ok_or_else(|| format!("local branch {branch:?} is not visible"))?;
        let expected_checkout = self
            .commits
            .values()
            .find(|commit| commit.local_refs.iter().any(|name| name == base))
            .map(|commit| commit.id.clone())
            .ok_or_else(|| format!("local base branch {base:?} is not visible"))?;
        let branches = if lower_stack {
            let mut branches = Vec::new();
            let mut current = source.id.clone();
            loop {
                let commit = self
                    .commits
                    .get(&current)
                    .ok_or_else(|| format!("unknown commit {current}"))?;
                let layer_branches: Vec<_> = commit
                    .local_refs
                    .iter()
                    .filter(|name| name.starts_with("fs-head/"))
                    .cloned()
                    .collect();
                if layer_branches.is_empty() {
                    break;
                }
                branches.extend(layer_branches);
                if commit.parents.len() != 1 {
                    break;
                }
                current = commit.parents[0].clone();
            }
            branches.sort();
            branches.dedup();
            branches
        } else {
            vec![branch.to_owned()]
        };
        if !branches.iter().any(|candidate| candidate == branch) {
            return Err("checked-out ForkStack branch is not on its HEAD commit".into());
        }
        let expected_branches = branches
            .iter()
            .map(|branch| {
                self.commits
                    .values()
                    .find(|commit| commit.local_refs.contains(branch))
                    .map(|commit| (branch.clone(), commit.id.clone()))
                    .ok_or_else(|| format!("local branch {branch:?} is not visible"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let remote_branches = ["origin", "upstream"]
            .into_iter()
            .flat_map(|remote| {
                branches.iter().flat_map(move |branch| {
                    let identity = branch.trim_start_matches("fs-head/");
                    ["fs-head", "fs-base"]
                        .into_iter()
                        .map(move |kind| (remote.to_owned(), format!("{kind}/{identity}")))
                })
            })
            .collect();
        Ok(DeleteBranchPlan {
            branch: branch.to_owned(),
            branches,
            expected_branches,
            current_branch: self.branch.clone(),
            expected_head: self.head.clone(),
            checkout_branch: base.to_owned(),
            expected_checkout,
            remote_branches,
        })
    }

    pub fn delete_branch_preview(&self, plan: &DeleteBranchPlan) -> Result<Self, String> {
        if self.branch != plan.current_branch || self.head != plan.expected_head {
            return Err("checkout changed while preparing deletion".into());
        }
        let mut graph = self.clone();
        let remote_refs: HashSet<_> = plan
            .remote_branches
            .iter()
            .map(|(remote, branch)| format!("{remote}/{branch}"))
            .collect();
        let local_refs: HashSet<_> = plan.branches.iter().collect();
        let deletes_current = plan
            .current_branch
            .as_ref()
            .is_some_and(|branch| plan.branches.contains(branch));
        for commit in graph.commits.values_mut() {
            commit.local_refs.retain(|name| !local_refs.contains(name));
            commit
                .remote_refs
                .retain(|name| !remote_refs.contains(name));
            if deletes_current {
                commit.is_head = false;
            }
        }
        if !deletes_current {
            graph.retain_reachable();
            return Ok(graph);
        }
        let checkout = graph
            .commits
            .get_mut(&plan.expected_checkout)
            .ok_or("local base branch moved outside the visible graph")?;
        checkout.is_head = true;
        checkout.preview = true;
        graph.head = plan.expected_checkout.clone();
        graph.branch = Some(plan.checkout_branch.clone());
        graph.retain_reachable();
        Ok(graph)
    }

    pub fn reset_local_preview(&self, remote: &str) -> (Self, usize, usize) {
        let mut graph = self.clone();
        let remote_prefix = format!("{remote}/fs-head/");
        let remote_targets: HashMap<String, String> = self
            .commits
            .iter()
            .flat_map(|(id, commit)| {
                commit.remote_refs.iter().filter_map(|name| {
                    name.strip_prefix(&remote_prefix)
                        .map(|tail| (format!("fs-head/{tail}"), id.clone()))
                })
            })
            .collect();
        let local_targets: HashMap<String, String> = self
            .commits
            .iter()
            .flat_map(|(id, commit)| {
                commit
                    .local_refs
                    .iter()
                    .filter(|name| name.starts_with("fs-head/"))
                    .map(|name| (name.clone(), id.clone()))
            })
            .collect();
        let updates: Vec<_> = local_targets
            .iter()
            .filter_map(|(branch, old)| {
                let target = remote_targets.get(branch)?;
                (old != target).then(|| (branch.clone(), target.clone()))
            })
            .collect();
        let missing = local_targets
            .keys()
            .filter(|branch| !remote_targets.contains_key(*branch))
            .count();
        let moved: HashSet<_> = updates.iter().map(|(branch, _)| branch.clone()).collect();

        for commit in graph.commits.values_mut() {
            commit.local_refs.retain(|name| !moved.contains(name));
        }
        for (branch, target) in &updates {
            if let Some(commit) = graph.commits.get_mut(target) {
                commit.local_refs.push(branch.clone());
                commit.local_refs.sort();
                commit.preview = true;
            }
        }
        if let Some((_, target)) = updates
            .iter()
            .find(|(branch, _)| Some(branch) == graph.branch.as_ref())
        {
            if let Some(commit) = graph.commits.get_mut(&graph.head) {
                commit.is_head = false;
            }
            graph.head = target.clone();
            if let Some(commit) = graph.commits.get_mut(target) {
                commit.is_head = true;
            }
        }
        graph.retain_reachable();
        (graph, updates.len(), missing)
    }

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

    fn children(&self) -> HashMap<String, Vec<String>> {
        let mut children: HashMap<String, Vec<String>> = self
            .commits
            .keys()
            .map(|id| (id.clone(), Vec::new()))
            .collect();
        for commit in self.commits.values() {
            for parent in &commit.parents {
                if let Some(parent_children) = children.get_mut(parent) {
                    parent_children.push(commit.id.clone());
                }
            }
        }
        children
    }

    /// Return commits reachable by following child edges from `ancestor`.
    fn descendants(&self, ancestor: &str) -> HashSet<String> {
        let children = self.children();
        let mut descendants = HashSet::new();
        let mut pending = vec![ancestor.to_owned()];
        while let Some(id) = pending.pop() {
            if !descendants.insert(id.clone()) {
                continue;
            }
            if let Some(next) = children.get(&id) {
                pending.extend(next.iter().cloned());
            }
        }
        descendants
    }

    fn descendant_forkstack_candidates(
        &self,
        ancestor: &str,
        excluded: &HashSet<String>,
    ) -> HashMap<String, Vec<String>> {
        let descendants = self.descendants(ancestor);
        self.commits
            .values()
            .filter(|commit| descendants.contains(&commit.id) && !excluded.contains(&commit.id))
            .filter_map(|commit| {
                let names: Vec<_> = commit
                    .local_refs
                    .iter()
                    .filter(|name| name.starts_with("fs-head/"))
                    .cloned()
                    .collect();
                (!names.is_empty()).then(|| (commit.id.clone(), names))
            })
            .collect()
    }

    /// Candidate commits with no candidate descendant.
    ///
    /// Propagating a single bit from leaves to roots avoids running a separate
    /// ancestry walk for every candidate. The traversal derives its topology
    /// from `commits`, rather than the UI's display order.
    fn maximal_candidates<'a>(
        &'a self,
        candidates: &'a HashMap<String, Vec<String>>,
    ) -> Vec<&'a String> {
        let children = self.children();
        let mut remaining_children: HashMap<_, _> = children
            .iter()
            .map(|(id, children)| (id.clone(), children.len()))
            .collect();
        let mut pending: Vec<_> = remaining_children
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(id, _)| id.clone())
            .collect();
        let mut contains_candidate: HashSet<_> = candidates.keys().cloned().collect();
        let mut has_candidate_descendant = HashSet::new();
        while let Some(id) = pending.pop() {
            let carries_candidate = contains_candidate.contains(&id);
            if let Some(commit) = self.commits.get(&id) {
                for parent in &commit.parents {
                    let Some(remaining) = remaining_children.get_mut(parent) else {
                        continue;
                    };
                    if carries_candidate {
                        contains_candidate.insert(parent.clone());
                        has_candidate_descendant.insert(parent.clone());
                    }
                    *remaining -= 1;
                    if *remaining == 0 {
                        pending.push(parent.clone());
                    }
                }
            }
        }
        candidates
            .keys()
            .filter(|id| !has_candidate_descendant.contains(*id))
            .collect()
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

        let candidates = self.descendant_forkstack_candidates(&self.head, &HashSet::new());
        let maximal = self.maximal_candidates(&candidates);
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

    pub fn plan_descendant_replay(&self, onto: &str) -> Result<Option<ReplayPlan>, String> {
        self.branch
            .as_ref()
            .filter(|branch| branch.starts_with("fs-head/"))
            .ok_or("check out a Forkstack fs-head branch before amending")?;
        let (tip, tip_commit, _) = self.stack_tip(&self.head)?;
        let commits = self.linear_segment(&self.head, &tip_commit)?;
        if commits.is_empty() {
            return Ok(None);
        }
        Ok(Some(ReplayPlan {
            onto: onto.to_owned(),
            upstream: self.head.clone(),
            branch: tip,
            ref_updates: self.ref_updates(&commits),
            commits,
        }))
    }

    fn descendant_stack_tip(
        &self,
        destination: &str,
        excluded: &HashSet<String>,
    ) -> Result<(String, String), String> {
        let candidates = self.descendant_forkstack_candidates(destination, excluded);
        let maximal = self.maximal_candidates(&candidates);
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

    fn direct_commit_branch(&self, selected: &str) -> Result<String, String> {
        let commit = self
            .commits
            .get(selected)
            .ok_or_else(|| format!("unknown commit {selected}"))?;
        let mut branches = commit.local_refs.clone();
        branches.sort();
        if let Some(current) = self
            .branch
            .as_ref()
            .filter(|current| branches.contains(current))
        {
            return Ok(current.clone());
        }
        branches.into_iter().next().ok_or_else(|| {
            "an exact single-commit move requires a local branch on the selected commit".into()
        })
    }

    /// Rebase exactly the selected commit or substack onto `destination`.
    pub fn plan_move(
        &self,
        selected: &str,
        destination: &str,
        include_descendants: bool,
    ) -> Result<MovePlan, String> {
        if selected == destination {
            return Err("a commit cannot be dropped onto itself".into());
        }
        let parent = self.first_parent(selected)?;
        if destination == parent {
            return Err("that move would not change the stack".into());
        }
        if self.is_ancestor(selected, destination) {
            return Err("the destination descends from the selected commit".into());
        }
        let (tip, tip_commit, detach_for_rewrite, commits) = if include_descendants {
            let (tip, tip_commit, detach_for_rewrite) = self.stack_tip(selected)?;
            let commits = self.linear_segment(&parent, &tip_commit)?;
            (tip, tip_commit, detach_for_rewrite, commits)
        } else {
            let tip = self.direct_commit_branch(selected)?;
            let detach_for_rewrite = tip.starts_with("fs-head/");
            (
                tip,
                selected.to_owned(),
                detach_for_rewrite,
                vec![selected.into()],
            )
        };
        let checkout_branch = tip.clone();
        let ref_updates = self.ref_updates(&commits);
        Ok(MovePlan {
            selected: selected.into(),
            destination: destination.into(),
            include_descendants,
            base: destination.into(),
            source_base: parent,
            carried_count: commits.len(),
            mode: MoveMode::Direct,
            tip,
            tip_commit,
            detach_for_rewrite,
            checkout_branch,
            ref_updates,
            commits,
        })
    }

    /// Insert the selected commit or substack after `destination`, replaying
    /// the rest of the destination stack above it.
    pub fn plan_reorder(
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
            let carried_ids: HashSet<_> = carried.iter().cloned().collect();
            let (destination_tip, destination_tip_commit) =
                self.descendant_stack_tip(destination, &carried_ids)?;
            let mut destination_descendants =
                self.linear_segment(destination, &destination_tip_commit)?;
            destination_descendants.retain(|id| !carried_ids.contains(id));
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
                mode: MoveMode::Reorder,
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
            mode: MoveMode::Reorder,
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
        if let Some(commit) = graph.commits.get_mut(&graph.head) {
            commit.is_head = false;
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
    fn plans_single_commit_and_substack_reorders() {
        let mut graph = graph();
        let single = graph.plan_reorder("b", "c", false).unwrap();
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
        let substack = graph.plan_reorder("c", "x", true).unwrap();
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

        let plan = graph.plan_reorder("b", "c", false).unwrap();
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
        let plan = graph.plan_reorder("b", "c", false).unwrap();
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
    fn delete_preview_removes_the_current_forkstack_refs_and_checks_out_base() {
        let mut graph = graph();
        graph.commits.get_mut("c").unwrap().local_refs = vec!["fs-head/topic/1".into()];
        graph.commits.get_mut("c").unwrap().remote_refs = vec![
            "origin/fs-head/topic/1".into(),
            "upstream/fs-head/topic/1".into(),
        ];
        graph.commits.get_mut("b").unwrap().remote_refs = vec![
            "origin/fs-base/topic/1".into(),
            "upstream/fs-base/topic/1".into(),
        ];
        graph.commits.get_mut("d").unwrap().is_head = false;
        graph.commits.get_mut("c").unwrap().is_head = true;
        graph.head = "c".into();
        graph.branch = Some("fs-head/topic/1".into());

        let plan = graph.plan_delete_current_branch("main").unwrap();
        let preview = graph.delete_branch_preview(&plan).unwrap();

        assert_eq!(plan.branch, "fs-head/topic/1");
        assert_eq!(plan.expected_head, "c");
        assert_eq!(plan.checkout_branch, "main");
        assert_eq!(plan.expected_checkout, "d");
        assert_eq!(
            plan.remote_branches,
            [
                ("origin".into(), "fs-head/topic/1".into()),
                ("origin".into(), "fs-base/topic/1".into()),
                ("upstream".into(), "fs-head/topic/1".into()),
                ("upstream".into(), "fs-base/topic/1".into()),
            ]
        );
        assert_eq!(preview.branch.as_deref(), Some("main"));
        assert_eq!(preview.head, "d");
        assert!(preview.commits["d"].is_head);
        assert!(preview.commits["d"].preview);
        assert!(preview.commits.values().all(|commit| {
            !commit
                .local_refs
                .iter()
                .any(|name| name == "fs-head/topic/1")
                && !commit.remote_refs.iter().any(|name| {
                    name.ends_with("/fs-head/topic/1") || name.ends_with("/fs-base/topic/1")
                })
        }));
    }

    #[test]
    fn delete_preview_requires_a_checked_out_forkstack_head() {
        let graph = graph();

        assert_eq!(
            graph.plan_delete_current_branch("main").unwrap_err(),
            "the checked-out branch is not a ForkStack fs-head branch"
        );
    }

    #[test]
    fn lower_stack_delete_includes_current_and_ancestor_layers_only() {
        let mut graph = Graph {
            commits: [
                (
                    "old-base".into(),
                    Commit {
                        id: "old-base".into(),
                        ..Commit::default()
                    },
                ),
                (
                    "main".into(),
                    Commit {
                        id: "main".into(),
                        parents: vec!["old-base".into()],
                        local_refs: vec!["main".into()],
                        ..Commit::default()
                    },
                ),
                (
                    "lower".into(),
                    Commit {
                        id: "lower".into(),
                        parents: vec!["old-base".into()],
                        local_refs: vec!["fs-head/topic/1".into()],
                        ..Commit::default()
                    },
                ),
                (
                    "current".into(),
                    Commit {
                        id: "current".into(),
                        parents: vec!["lower".into()],
                        local_refs: vec!["fs-head/topic/2".into()],
                        ..Commit::default()
                    },
                ),
                (
                    "upper".into(),
                    Commit {
                        id: "upper".into(),
                        parents: vec!["current".into()],
                        local_refs: vec!["fs-head/topic/3".into()],
                        is_head: true,
                        ..Commit::default()
                    },
                ),
            ]
            .into(),
            order: vec![
                "upper".into(),
                "current".into(),
                "lower".into(),
                "main".into(),
                "old-base".into(),
            ],
            head: "upper".into(),
            branch: Some("fs-head/topic/3".into()),
        };
        for commit in graph.commits.values_mut() {
            for remote in ["origin", "upstream"] {
                for branch in &commit.local_refs {
                    if branch.starts_with("fs-head/") {
                        commit.remote_refs.push(format!("{remote}/{branch}"));
                    }
                }
            }
        }

        let plan = graph
            .plan_delete_branches("fs-head/topic/2", "main", true)
            .unwrap();
        let preview = graph.delete_branch_preview(&plan).unwrap();

        assert_eq!(plan.branches, ["fs-head/topic/1", "fs-head/topic/2"]);
        assert_eq!(plan.remote_branches.len(), 8);
        assert!(preview.commits["lower"].local_refs.is_empty());
        assert!(preview.commits["current"].local_refs.is_empty());
        assert_eq!(preview.commits["upper"].local_refs, ["fs-head/topic/3"]);
        assert_eq!(preview.branch.as_deref(), Some("fs-head/topic/3"));
        assert_eq!(preview.head, "upper");
    }

    #[test]
    fn local_reset_preview_moves_only_heads_with_matching_remote_refs() {
        let mut graph = graph();
        graph.commits.get_mut("b").unwrap().local_refs =
            vec!["fs-head/topic/1".into(), "fs-head/missing/1".into()];
        graph.commits.get_mut("d").unwrap().remote_refs = vec!["origin/fs-head/topic/1".into()];
        graph.head = "b".into();
        graph.branch = Some("fs-head/topic/1".into());
        graph.commits.get_mut("b").unwrap().is_head = true;

        let (preview, updated, missing) = graph.reset_local_preview("origin");

        assert_eq!((updated, missing), (1, 1));
        assert_eq!(preview.head, "d");
        assert!(preview.commits["d"].is_head);
        assert!(preview.commits["d"].preview);
        assert!(
            preview.commits["d"]
                .local_refs
                .contains(&"fs-head/topic/1".into())
        );
        assert!(
            preview.commits["b"]
                .local_refs
                .contains(&"fs-head/missing/1".into())
        );
        assert!(!preview.commits["b"].is_head);
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
        let plan = graph.plan_reorder("b", "d", false).unwrap();
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

        let plan = graph.plan_reorder("c", "d", false).unwrap();
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

    fn linear_forkstack_graph() -> Graph {
        let entries = [
            ("main", None, Some("main")),
            ("draft3", Some("main"), Some("fs-head/draft/3")),
            ("draft7", Some("draft3"), Some("fs-head/draft/7")),
            ("draft6", Some("draft7"), Some("fs-head/draft/6")),
            ("draft5", Some("draft6"), Some("fs-head/draft/5")),
            ("rdma2", Some("draft5"), Some("fs-head/rdma/2")),
            ("rdma3", Some("rdma2"), Some("fs-head/rdma/3")),
        ];
        let commits = entries
            .into_iter()
            .map(|(id, parent, branch)| {
                (
                    id.into(),
                    Commit {
                        id: id.into(),
                        parents: parent.into_iter().map(str::to_owned).collect(),
                        subject: id.into(),
                        local_refs: branch.into_iter().map(str::to_owned).collect(),
                        is_head: id == "rdma3",
                        ..Commit::default()
                    },
                )
            })
            .collect();
        Graph {
            commits,
            order: entries
                .iter()
                .rev()
                .map(|(id, _, _)| (*id).into())
                .collect(),
            head: "rdma3".into(),
            branch: Some("fs-head/rdma/3".into()),
        }
    }

    #[test]
    fn direct_moves_create_parallel_trees() {
        let graph = linear_forkstack_graph();

        let single = graph.plan_move("rdma2", "main", false).unwrap();
        assert_eq!(single.mode, MoveMode::Direct);
        assert_eq!(single.commits, ["rdma2"]);
        assert_eq!(single.checkout_branch, "fs-head/rdma/2");
        let single_preview = graph.preview(&single).unwrap();
        assert_eq!(single_preview.commits["preview:rdma2"].parents, ["main"]);
        assert_eq!(single_preview.head, "preview:rdma2");
        assert!(!single_preview.commits["rdma3"].is_head);
        assert!(
            single_preview.commits["rdma3"]
                .local_refs
                .contains(&"fs-head/rdma/3".into())
        );

        let substack = graph.plan_move("rdma2", "main", true).unwrap();
        assert_eq!(substack.mode, MoveMode::Direct);
        assert_eq!(substack.commits, ["rdma2", "rdma3"]);
        assert_eq!(substack.checkout_branch, "fs-head/rdma/3");
        let substack_preview = graph.preview(&substack).unwrap();
        assert_eq!(substack_preview.commits["preview:rdma2"].parents, ["main"]);
        assert_eq!(
            substack_preview.commits["preview:rdma3"].parents,
            ["preview:rdma2"]
        );
        assert!(
            substack_preview.commits["draft5"]
                .local_refs
                .contains(&"fs-head/draft/5".into())
        );
    }

    #[test]
    fn substack_reorder_places_the_remaining_destination_tree_on_top() {
        let graph = linear_forkstack_graph();

        let plan = graph.plan_reorder("rdma2", "main", true).unwrap();

        assert_eq!(plan.mode, MoveMode::Reorder);
        assert_eq!(plan.carried_count, 2);
        assert_eq!(
            plan.commits,
            ["rdma2", "rdma3", "draft3", "draft7", "draft6", "draft5"]
        );
        assert_eq!(plan.checkout_branch, "fs-head/draft/5");
        let preview = graph.preview(&plan).unwrap();
        assert_eq!(preview.commits["preview:rdma2"].parents, ["main"]);
        assert_eq!(preview.commits["preview:rdma3"].parents, ["preview:rdma2"]);
        assert_eq!(preview.commits["preview:draft3"].parents, ["preview:rdma3"]);
        assert_eq!(preview.head, "preview:draft5");
    }

    #[test]
    fn substack_insertion_replays_destination_descendants_above_carried_commits() {
        let graph = insertion_graph();
        assert_eq!(graph.carried_substack("beta2").unwrap(), ["beta2", "beta3"]);
        let plan = graph.plan_reorder("beta3", "alpha2", true).unwrap();

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

        let error = graph.plan_reorder("beta3", "alpha2", true).unwrap_err();
        assert!(error.contains("multiple descendant stack tips"));
    }
}
