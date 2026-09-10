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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKind {
    CommitHash,
    Head,
    LocalRef,
    RemoteRef,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledRange {
    pub start: usize,
    pub end: usize,
    pub kind: TextKind,
}

#[derive(Clone, Debug)]
pub struct RenderedLine {
    pub text: String,
    pub commit: Option<String>,
    pub preview: bool,
    pub conflict: bool,
    pub links: Vec<RenderedLink>,
    pub styles: Vec<StyledRange>,
}

fn short_id(id: &str) -> &str {
    let id = id.strip_prefix("preview:").unwrap_or(id);
    &id[..id.len().min(8)]
}

fn push_styled(text: &mut String, styles: &mut Vec<StyledRange>, value: &str, kind: TextKind) {
    let start = text.len();
    text.push_str(value);
    styles.push(StyledRange {
        start,
        end: text.len(),
        kind,
    });
}

fn commit_label_with_styles(
    commit: &Commit,
    head_branch: Option<&str>,
) -> (String, Vec<StyledRange>) {
    let mut text = String::new();
    let mut styles = Vec::new();
    push_styled(
        &mut text,
        &mut styles,
        short_id(&commit.id),
        TextKind::CommitHash,
    );

    let mut labels: Vec<(String, Vec<StyledRange>)> = Vec::new();
    if commit.is_head {
        if let Some(branch) = head_branch {
            let mut label = String::new();
            let mut label_styles = Vec::new();
            push_styled(&mut label, &mut label_styles, "HEAD", TextKind::Head);
            label.push_str(" -> ");
            push_styled(&mut label, &mut label_styles, branch, TextKind::LocalRef);
            labels.push((label, label_styles));
        } else {
            labels.push((
                "HEAD".into(),
                vec![StyledRange {
                    start: 0,
                    end: 4,
                    kind: TextKind::Head,
                }],
            ));
        }
    }
    for name in &commit.local_refs {
        if !commit.is_head || Some(name.as_str()) != head_branch {
            labels.push((
                name.clone(),
                vec![StyledRange {
                    start: 0,
                    end: name.len(),
                    kind: TextKind::LocalRef,
                }],
            ));
        }
    }
    labels.extend(commit.remote_refs.iter().map(|name| {
        (
            name.clone(),
            vec![StyledRange {
                start: 0,
                end: name.len(),
                kind: TextKind::RemoteRef,
            }],
        )
    }));
    labels.extend(commit.tags.iter().map(|tag| {
        let label = format!("tag: {tag}");
        let end = label.len();
        (
            label,
            vec![StyledRange {
                start: 0,
                end,
                kind: TextKind::Tag,
            }],
        )
    }));
    if let Some(conflict) = &commit.conflict {
        labels.push((conflict.clone(), Vec::new()));
    }
    if !labels.is_empty() {
        text.push_str(" (");
        for (index, (label, label_styles)) in labels.into_iter().enumerate() {
            if index != 0 {
                text.push_str(", ");
            }
            let offset = text.len();
            text.push_str(&label);
            styles.extend(label_styles.into_iter().map(|style| StyledRange {
                start: offset + style.start,
                end: offset + style.end,
                kind: style.kind,
            }));
        }
        text.push(')');
    }
    text.push(' ');
    text.push_str(&commit.subject);
    (text, styles)
}

pub fn commit_label(commit: &Commit, head_branch: Option<&str>) -> String {
    commit_label_with_styles(commit, head_branch).0
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
        let (label, label_styles) = commit_label_with_styles(commit, graph.branch.as_deref());
        let row = renderer.next_row(id.clone(), parents, glyph.into(), label.clone());
        for (index, text) in row.lines().enumerate() {
            let styles = if index == 0 {
                text.find(&label)
                    .map(|offset| {
                        label_styles
                            .iter()
                            .map(|style| StyledRange {
                                start: offset + style.start,
                                end: offset + style.end,
                                kind: style.kind,
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            lines.push(RenderedLine {
                text: text.into(),
                commit: (index == 0).then(|| id.clone()),
                preview: commit.preview,
                conflict: commit.conflict.is_some(),
                links: Vec::new(),
                styles,
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
    fn graph_marks_only_git_decoration_ranges_for_semantic_colors() {
        let commit = Commit {
            id: "abcdef012345".into(),
            subject: "subject remains plain".into(),
            local_refs: vec!["topic".into(), "main".into()],
            remote_refs: vec!["origin/topic".into()],
            tags: vec!["v1".into()],
            is_head: true,
            ..Commit::default()
        };
        let graph = Graph {
            commits: [(commit.id.clone(), commit.clone())].into(),
            order: vec![commit.id.clone()],
            branch: Some("main".into()),
            ..Graph::default()
        };

        let rendered = render_graph(&graph);
        let styled = rendered[0]
            .styles
            .iter()
            .map(|range| (&rendered[0].text[range.start..range.end], range.kind))
            .collect::<Vec<_>>();

        assert_eq!(
            styled,
            [
                ("abcdef01", TextKind::CommitHash),
                ("HEAD", TextKind::Head),
                ("main", TextKind::LocalRef),
                ("topic", TextKind::LocalRef),
                ("origin/topic", TextKind::RemoteRef),
                ("tag: v1", TextKind::Tag),
            ]
        );
        assert!(!styled.iter().any(|(text, _)| text.contains("subject")));
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
