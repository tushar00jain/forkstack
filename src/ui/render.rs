use renderdag::{Ancestor, GraphRowRenderer, Renderer};

use crate::ui::model::{Commit, Graph};

#[derive(Clone, Debug)]
pub struct RenderedLine {
    pub text: String,
    pub commit: Option<String>,
    pub preview: bool,
    pub conflict: bool,
}

fn short_id(id: &str) -> &str {
    let id = id.strip_prefix("preview:").unwrap_or(id);
    &id[..id.len().min(8)]
}

pub fn commit_label(commit: &Commit, head_branch: Option<&str>) -> String {
    let mut labels = Vec::new();
    if commit.is_head {
        if let Some(branch) = head_branch {
            labels.push(format!("HEAD -> {branch}"));
        } else {
            labels.push("HEAD".into());
        }
    }
    for name in &commit.local_refs {
        if !commit.is_head || Some(name.as_str()) != head_branch {
            labels.push(name.clone());
        }
    }
    labels.extend(commit.remote_refs.iter().cloned());
    labels.extend(commit.tags.iter().map(|tag| format!("tag: {tag}")));
    if let Some(conflict) = &commit.conflict {
        labels.push(conflict.clone());
    }
    let decoration = if labels.is_empty() {
        String::new()
    } else {
        format!(" ({})", labels.join(", "))
    };
    format!("{}{} {}", short_id(&commit.id), decoration, commit.subject)
}

pub fn render_graph(graph: &Graph) -> Vec<RenderedLine> {
    let mut renderer = GraphRowRenderer::<String>::new()
        .output()
        .with_min_row_height(1)
        .build_box_drawing();
    let mut lines = Vec::new();
    for id in &graph.order {
        let Some(commit) = graph.commits.get(id) else {
            continue;
        };
        let parents = commit
            .parents
            .iter()
            .cloned()
            .map(Ancestor::Parent)
            .collect();
        let glyph = if commit.conflict.is_some() {
            "x"
        } else if commit.preview {
            "◆"
        } else {
            "o"
        };
        let row = renderer.next_row(
            id.clone(),
            parents,
            glyph.into(),
            commit_label(commit, graph.branch.as_deref()),
        );
        for (index, text) in row.lines().enumerate() {
            lines.push(RenderedLine {
                text: text.into(),
                commit: (index == 0).then(|| id.clone()),
                preview: commit.preview,
                conflict: commit.conflict.is_some(),
            });
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_decoration_uses_actual_branch_not_sorted_first_ref() {
        let commit = Commit {
            id: "abcdef012345".into(),
            subject: "subject".into(),
            local_refs: vec!["aaa-marker".into(), "main".into()],
            is_head: true,
            ..Commit::default()
        };
        let label = commit_label(&commit, Some("main"));
        assert!(label.contains("HEAD -> main"));
        assert!(label.contains("aaa-marker"));
        assert_eq!(label.matches("main").count(), 1);
    }

    #[test]
    fn preview_uses_styling_without_label_suffix() {
        let commit = Commit {
            id: "preview:abcdef012345".into(),
            subject: "subject".into(),
            preview: true,
            ..Commit::default()
        };
        let graph = Graph {
            commits: [(commit.id.clone(), commit.clone())].into(),
            order: vec![commit.id.clone()],
            ..Graph::default()
        };

        assert_eq!(commit_label(&commit, None), "abcdef01 subject");
        let rendered = render_graph(&graph);
        assert!(rendered[0].preview);
        assert!(rendered[0].text.contains('◆'));
        assert!(!rendered[0].text.contains("[preview]"));
    }
}
