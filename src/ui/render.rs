use std::collections::BTreeMap;

use ratatui::text::Line;
use renderdag::{Ancestor, GraphRowRenderer, Renderer};

use crate::integrations::github::PullRequestLink;
use crate::ui::model::{Commit, Graph};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedLink {
    pub start: usize,
    pub width: usize,
    pub text: String,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct RenderedLine {
    pub text: String,
    pub commit: Option<String>,
    pub preview: bool,
    pub conflict: bool,
    pub links: Vec<RenderedLink>,
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
                links: Vec::new(),
            });
        }
    }
    lines
}

pub fn attach_pr_links(
    lines: &mut [RenderedLine],
    graph: &Graph,
    links: &BTreeMap<String, PullRequestLink>,
) {
    for line in lines {
        line.links.clear();
        let Some(commit) = line.commit.as_ref().and_then(|id| graph.commits.get(id)) else {
            continue;
        };
        for name in &commit.remote_refs {
            let Some(pr) = links.get(name) else {
                continue;
            };
            let Some(byte_start) = line.text.find(name) else {
                continue;
            };
            line.links.push(RenderedLink {
                start: Line::raw(&line.text[..byte_start]).width(),
                width: Line::raw(name).width(),
                text: name.clone(),
                url: pr.url.clone(),
            });
        }
        line.links.sort_by_key(|link| link.start);
    }
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

    #[test]
    fn existing_remote_ref_text_gets_a_link_without_extra_label_text() {
        let commit = Commit {
            id: "abcdef012345".into(),
            subject: "subject".into(),
            remote_refs: vec![
                "origin/fs-base/topic/1".into(),
                "origin/fs-head/topic/1".into(),
            ],
            ..Commit::default()
        };
        let graph = Graph {
            commits: [(commit.id.clone(), commit.clone())].into(),
            order: vec![commit.id.clone()],
            ..Graph::default()
        };
        let original = commit_label(&commit, None);
        let mut rendered = render_graph(&graph);
        attach_pr_links(
            &mut rendered,
            &graph,
            &[(
                ("origin/fs-head/topic/1").into(),
                PullRequestLink {
                    number: 9,
                    head_ref_name: "fs-head/topic/1".into(),
                    url: "https://example.invalid/9".into(),
                },
            )]
            .into(),
        );

        assert!(rendered[0].text.ends_with(&original));
        assert_eq!(rendered[0].links.len(), 1);
        assert_eq!(rendered[0].links[0].text, "origin/fs-head/topic/1");
        assert_eq!(rendered[0].links[0].url, "https://example.invalid/9");
    }
}
