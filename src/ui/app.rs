use std::collections::{BTreeMap, HashSet};
use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use crossterm::cursor::MoveTo;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{
    Attribute as TerminalAttribute, Color as TerminalColor, Colors, Print, SetAttribute, SetColors,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::core::submit::{SubmitOptions, SubmitPlan};
use crate::integrations::github::PullRequestLink;
use crate::ui::event::{self, Operation, OperationRequest, OperationResult, UiEvent};
use crate::ui::model::{Graph, MovePlan};
use crate::ui::render::{RenderedLine, TextKind, attach_pr_links, commit_label, render_graph};
use crate::ui::workspace::{self, Workspace};

type Tui = Terminal<CrosstermBackend<Stdout>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Busy {
    Load,
    Mutation,
}

#[derive(Default)]
struct Search {
    editing: bool,
    query: String,
    matches: Vec<String>,
}

fn move_pick(key: &KeyEvent) -> Option<bool> {
    match key.code {
        KeyCode::Char('m') => Some(false),
        KeyCode::Char('M') => Some(true),
        _ => None,
    }
}

fn preserve_preview_while_navigating(publish_active: bool) -> bool {
    publish_active
}

fn checkout_branch(graph: &Graph, commit: &str) -> Option<String> {
    let refs = &graph.commits.get(commit)?.local_refs;
    (refs.len() == 1).then(|| refs[0].clone())
}

fn help_transition(open: bool, key: &KeyEvent) -> Option<bool> {
    if open {
        Some(!matches!(key.code, KeyCode::Char('?') | KeyCode::Esc))
    } else {
        (key.code == KeyCode::Char('?')).then_some(true)
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn key_line<'a>(bindings: &'a [(&'a str, &'a str)]) -> Line<'a> {
    let key_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(Color::White);
    let mut spans = Vec::new();
    for (index, (key, label)) in bindings.iter().enumerate() {
        if index != 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(*key, key_style));
        spans.push(Span::styled(format!(" {label}"), label_style));
    }
    Line::from(spans)
}

fn compact_footer() -> Line<'static> {
    key_line(&[
        ("Tab/⇧Tab", "pane"),
        ("↑/↓", "select"),
        ("Enter", "open/confirm"),
        ("?", "help"),
        ("q", "quit"),
    ])
}

fn help_line(key: &str, label: &str) -> Line<'static> {
    const KEY_COLUMN_WIDTH: usize = 13;
    let key_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let padding = KEY_COLUMN_WIDTH.saturating_sub(Line::raw(key).width());
    Line::from(vec![
        Span::styled(key.to_owned(), key_style),
        Span::raw(" ".repeat(padding)),
        Span::styled(label.to_owned(), Style::default().fg(Color::White)),
    ])
}

fn help_lines() -> Vec<Line<'static>> {
    vec![
        help_line("Tab/⇧Tab", "focus next/previous pane"),
        help_line("↑/↓, j/k", "navigate focused pane"),
        help_line("Enter: repos", "activate highlighted repository"),
        help_line("Enter", "checkout or confirm preview"),
        help_line("m/M", "move commit / substack"),
        help_line("Esc", "exit search/filter, cancel preview/close help"),
        help_line("p", "preview publish and stack link"),
        help_line("u", "preview upstream publish and stack link"),
        help_line("/", "search focused pane"),
        help_line("n/N", "next/previous match"),
        help_line("r", "rescan; in graph also refresh graph/PRs"),
        help_line("?", "open or close help"),
        help_line("q", "quit"),
    ]
}

fn hyperlink_sequence(url: &str, text: &str) -> Option<String> {
    (!url.chars().any(char::is_control) && !text.chars().any(char::is_control))
        .then(|| format!("\x1B]8;;{url}\x07{text}\x1B]8;;\x07"))
}

#[derive(Debug)]
struct TerminalLink {
    x: u16,
    y: u16,
    text: String,
    url: String,
    style: Style,
}

fn write_terminal_links(writer: &mut impl Write, links: &[TerminalLink]) -> io::Result<()> {
    for link in links {
        let Some(sequence) = hyperlink_sequence(&link.url, &link.text) else {
            continue;
        };
        let foreground = link.style.fg.unwrap_or(Color::Reset).into();
        let background = link.style.bg.unwrap_or(Color::Reset).into();
        queue!(
            writer,
            MoveTo(link.x, link.y),
            SetAttribute(TerminalAttribute::Reset),
            SetColors(Colors::new(foreground, background))
        )?;
        if link.style.add_modifier.contains(Modifier::BOLD) {
            queue!(writer, SetAttribute(TerminalAttribute::Bold))?;
        }
        queue!(
            writer,
            Print(sequence),
            SetAttribute(TerminalAttribute::Reset),
            SetColors(Colors::new(TerminalColor::Reset, TerminalColor::Reset))
        )?;
    }
    writer.flush()
}

fn graph_line_style(
    line: &RenderedLine,
    selected: Option<&String>,
    carried_commits: &HashSet<String>,
) -> Style {
    let mut style = Style::default();
    if line.preview {
        style = style.fg(Color::Cyan);
    }
    if line.conflict {
        style = style.fg(Color::Red).add_modifier(Modifier::BOLD);
    }
    if line
        .commit
        .as_ref()
        .is_some_and(|id| carried_commits.contains(id))
    {
        style = style.fg(Color::Yellow);
    }
    if line.commit.as_ref() == selected {
        style = style.bg(Color::Rgb(64, 64, 64));
    }
    style
}

fn text_kind_style(mut base: Style, kind: TextKind) -> Style {
    base = match kind {
        TextKind::CommitHash | TextKind::Tag => base.fg(Color::Yellow),
        TextKind::Head => base.fg(Color::Cyan).add_modifier(Modifier::BOLD),
        TextKind::LocalRef => base.fg(Color::Green),
        TextKind::RemoteRef => base.fg(Color::Red),
    };
    base
}

fn styled_graph_line(line: &RenderedLine, base: Style) -> Line<'static> {
    let mut spans = Vec::new();
    let mut position = 0;
    for range in &line.styles {
        if position < range.start {
            spans.push(Span::styled(
                line.text[position..range.start].to_owned(),
                base,
            ));
        }
        spans.push(Span::styled(
            line.text[range.start..range.end].to_owned(),
            text_kind_style(base, range.kind),
        ));
        position = range.end;
    }
    if position < line.text.len() {
        spans.push(Span::styled(line.text[position..].to_owned(), base));
    }
    Line::from(spans)
}

#[derive(Default)]
struct RepositoryState {
    graph: Option<Graph>,
    preview: Option<Graph>,
    selected: Option<String>,
    carried: Option<String>,
    carried_commits: HashSet<String>,
    carry_substack: bool,
    pending: Option<MovePlan>,
    publish_plan: Option<SubmitPlan>,
    search: Search,
    status: Option<String>,
    status_error: bool,
    busy: Option<Busy>,
    scroll: usize,
    rendered: Vec<RenderedLine>,
    pr_links: BTreeMap<String, PullRequestLink>,
    request_id: u64,
    discard_preview: bool,
    render_dirty: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Repositories,
    Graph,
}

pub struct App {
    state: RepositoryState,
    cached: BTreeMap<PathBuf, RepositoryState>,
    workspace: Workspace,
    active: Option<PathBuf>,
    pane: Pane,
    repository_search: Search,
    repository_cursor: usize,
    repository_scroll: usize,
    next_request_id: u64,
    publish_options: SubmitOptions,
    operations: Sender<OperationRequest>,
    events: Receiver<UiEvent>,
    input_events: Option<Sender<UiEvent>>,
    quit: bool,
    help_open: bool,
    dirty: bool,
}

impl RepositoryState {
    fn active_graph(&self) -> Option<&Graph> {
        self.preview.as_ref().or(self.graph.as_ref())
    }

    fn graph_ids(&self) -> Vec<String> {
        self.active_graph()
            .map(|graph| graph.order.clone())
            .unwrap_or_default()
    }

    fn update_rendered(&mut self) {
        let rendered = self
            .active_graph()
            .map(|graph| {
                let mut rendered = render_graph(graph);
                attach_pr_links(&mut rendered, graph, &self.pr_links);
                rendered
            })
            .unwrap_or_default();
        self.rendered = rendered;
        self.render_dirty = false;
    }

    fn receive(&mut self, mut response: OperationResult, active: bool) {
        if response.id != self.request_id {
            return;
        }
        if self.discard_preview || !active {
            response.preview = None;
            response.publish_plan = None;
        }
        let mutation = self.busy == Some(Busy::Mutation);
        let updates_links = response.pr_links.is_some();
        self.busy = None;
        match response.graph {
            Ok(graph) => {
                let old = self.selected.clone();
                self.graph = Some(graph);
                self.preview = response.preview;
                self.publish_plan = response.publish_plan;
                if mutation {
                    self.carried = None;
                    self.carried_commits.clear();
                    self.pending = None;
                    self.publish_plan = None;
                }
                let ids = self.graph_ids();
                self.selected = old
                    .filter(|id| ids.contains(id))
                    .or_else(|| ids.first().cloned());
                self.status_error = response.operation_error.is_some();
                self.status = response.operation_error;
            }
            Err(error) => {
                if !updates_links {
                    self.pr_links.clear();
                }
                self.status = Some(response.operation_error.unwrap_or(error));
                self.status_error = true;
            }
        }
        if let Some(result) = response.pr_links {
            match result {
                Ok(links) => {
                    self.pr_links = links;
                }
                Err(error) => {
                    self.pr_links.clear();
                    if self.status.is_none() {
                        self.status = Some(error);
                        self.status_error = false;
                    }
                }
            }
        }
        self.render_dirty = true;
        if active {
            self.update_rendered();
        }
    }
}

impl App {
    pub fn new(publish_options: SubmitOptions) -> Result<Self, String> {
        let workspace = workspace::discover(&publish_options.repo)?;
        Ok(Self::with_workspace(
            publish_options,
            workspace,
            event::start(),
        ))
    }

    fn with_workspace(
        publish_options: SubmitOptions,
        workspace: Workspace,
        event_loop: event::EventLoop,
    ) -> Self {
        let initial = workspace.initial.clone();
        let mut app = Self {
            state: RepositoryState::default(),
            cached: BTreeMap::new(),
            workspace,
            active: None,
            pane: if initial.is_some() {
                Pane::Graph
            } else {
                Pane::Repositories
            },
            repository_search: Search::default(),
            repository_cursor: 0,
            repository_scroll: 0,
            next_request_id: 0,
            publish_options,
            operations: event_loop.operations,
            events: event_loop.events,
            input_events: Some(event_loop.input_events),
            quit: false,
            help_open: false,
            dirty: true,
        };
        if let Some(root) = initial {
            app.repository_cursor = app
                .workspace
                .repositories
                .iter()
                .position(|p| p == &root)
                .unwrap_or(0);
            app.activate(root);
        }
        app
    }

    fn has_sidebar(&self) -> bool {
        self.active.is_none()
            || self.workspace.repositories.len() != 1
            || self.workspace.initial.is_none()
    }

    fn filtered_repositories(&self) -> Vec<PathBuf> {
        let query = self.repository_search.query.to_lowercase();
        self.workspace
            .repositories
            .iter()
            .filter(|path| {
                let relative = path.strip_prefix(&self.workspace.root).unwrap_or(path);
                let name = if relative.as_os_str().is_empty() {
                    path.file_name().unwrap_or(path.as_os_str())
                } else {
                    relative.as_os_str()
                };
                name.to_string_lossy().to_lowercase().contains(&query)
                    || (query.contains('/')
                        && path.to_string_lossy().to_lowercase().contains(&query))
            })
            .cloned()
            .collect()
    }

    fn move_repository_cursor(&mut self, amount: isize) {
        let count = self.filtered_repositories().len();
        self.repository_cursor = if count == 0 {
            0
        } else {
            (self.repository_cursor as isize + amount).clamp(0, count as isize - 1) as usize
        };
    }

    fn activate(&mut self, root: PathBuf) {
        if self.state.busy == Some(Busy::Mutation) {
            self.reject_busy_operation();
            return;
        }
        if self.active.as_ref() != Some(&root) {
            self.clear_preview(false);
            if let Some(previous) = self.active.take() {
                self.cached
                    .insert(previous, std::mem::take(&mut self.state));
            }
            self.state = self.cached.remove(&root).unwrap_or_default();
            self.publish_options.repo = root.clone();
            self.active = Some(root);
            if self.state.render_dirty {
                self.update_rendered();
            }
            self.update_search();
        }
        if self.state.graph.is_none() && self.state.busy.is_none() {
            self.start_operation(Operation::Load, Busy::Load, "loading graph…");
        }
        self.dirty = true;
    }

    fn rescan(&mut self) -> bool {
        if self.state.busy == Some(Busy::Mutation) {
            self.reject_busy_operation();
            return false;
        }
        let highlighted = self
            .filtered_repositories()
            .get(self.repository_cursor)
            .cloned();
        match workspace::discover(&self.workspace.root) {
            Ok(workspace) => {
                self.workspace.repositories = workspace.repositories;
                self.cached
                    .retain(|root, _| self.workspace.repositories.contains(root));
                if self
                    .active
                    .as_ref()
                    .is_some_and(|root| !self.workspace.repositories.contains(root))
                {
                    self.active = None;
                    self.state = RepositoryState::default();
                    self.pane = Pane::Repositories;
                }
                let filtered = self.filtered_repositories();
                self.repository_cursor = highlighted
                    .and_then(|root| filtered.iter().position(|p| p == &root))
                    .unwrap_or(0);
                if !self.has_sidebar() && self.active.is_some() {
                    self.pane = Pane::Graph;
                }
                self.dirty = true;
                true
            }
            Err(error) => {
                self.state.status = Some(error);
                self.state.status_error = true;
                self.dirty = true;
                false
            }
        }
    }

    fn handle_repository_key(&mut self, key: KeyEvent) -> bool {
        if self.repository_search.editing {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.repository_search.editing = false,
                KeyCode::Up => self.move_repository_cursor(-1),
                KeyCode::Down => self.move_repository_cursor(1),
                KeyCode::Backspace => {
                    self.repository_search.query.pop();
                    self.repository_cursor = 0;
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.repository_search.query.push(character);
                    self.repository_cursor = 0;
                }
                _ => return false,
            }
            return true;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_repository_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_repository_cursor(1),
            KeyCode::Char('/') => {
                self.repository_search.editing = true;
                self.repository_search.query.clear();
                self.repository_cursor = 0;
            }
            KeyCode::Esc => {
                let highlighted = self
                    .filtered_repositories()
                    .get(self.repository_cursor)
                    .cloned();
                self.repository_search.query.clear();
                self.repository_cursor = highlighted
                    .and_then(|root| self.workspace.repositories.iter().position(|p| p == &root))
                    .unwrap_or(0);
            }
            KeyCode::Enter => {
                if let Some(root) = self
                    .filtered_repositories()
                    .get(self.repository_cursor)
                    .cloned()
                {
                    self.activate(root);
                }
            }
            KeyCode::Char('r') => {
                self.rescan();
            }
            _ => return false,
        }
        true
    }

    fn graph_ids(&self) -> Vec<String> {
        self.state.graph_ids()
    }

    fn update_rendered(&mut self) {
        self.state.update_rendered();
        self.dirty = true;
    }

    fn clear_preview(&mut self, keep_carried: bool) {
        self.state.discard_preview = true;
        let had_preview = self.state.preview.take().is_some();
        self.state.pending = None;
        self.state.publish_plan = None;
        if !keep_carried {
            self.state.carried = None;
            self.state.carried_commits.clear();
        }
        if had_preview {
            self.update_rendered();
        }
    }

    fn start_operation(&mut self, operation: Operation, busy: Busy, status: &str) -> bool {
        if self.state.busy.is_some() {
            self.reject_busy_operation();
            return false;
        }
        if self.active.is_none() {
            return false;
        }
        self.next_request_id += 1;
        self.state.request_id = self.next_request_id;
        self.state.discard_preview = false;
        let request = OperationRequest {
            options: self.publish_options.clone(),
            id: self.state.request_id,
            operation,
        };
        if self.operations.send(request).is_err() {
            self.state.status = Some("operation worker is unavailable".into());
            self.state.status_error = true;
            self.dirty = true;
            return false;
        }
        self.state.busy = Some(busy);
        self.state.status = Some(status.into());
        self.state.status_error = false;
        self.dirty = true;
        true
    }

    fn reject_busy_operation(&mut self) {
        self.state.status = Some("another operation is already running".into());
        self.state.status_error = true;
        self.dirty = true;
    }

    fn move_cursor(&mut self, amount: isize) {
        if !preserve_preview_while_navigating(self.state.publish_plan.is_some()) {
            self.clear_preview(true);
        }
        let ids = self.graph_ids();
        if ids.is_empty() {
            return;
        }
        let current = self
            .state
            .selected
            .as_ref()
            .and_then(|id| ids.iter().position(|item| item == id))
            .unwrap_or(0);
        let next = (current as isize + amount).clamp(0, ids.len() as isize - 1) as usize;
        self.state.selected = Some(ids[next].clone());
    }

    fn pick(&mut self, substack: bool) {
        let Some(selected) = self.state.selected.clone() else {
            return;
        };
        let commits = if substack {
            let Some(graph) = self.state.graph.as_ref() else {
                return;
            };
            match graph.carried_substack(&selected) {
                Ok(commits) => commits,
                Err(error) => {
                    self.clear_preview(false);
                    self.state.status = Some(error);
                    self.state.status_error = true;
                    return;
                }
            }
        } else {
            vec![selected.clone()]
        };
        self.clear_preview(false);
        self.state.carried = Some(selected);
        self.state.carried_commits = commits.into_iter().collect();
        self.state.carry_substack = substack;
        self.state.status = None;
        self.state.status_error = false;
    }

    fn enter(&mut self) {
        if self.state.pending.is_some() {
            self.apply();
            return;
        }
        if self.state.publish_plan.is_some() {
            self.execute_publish();
            return;
        }
        let Some(selected) = self.state.selected.clone() else {
            return;
        };
        if let Some(carried) = self.state.carried.clone() {
            let Some(graph) = self.state.graph.as_ref() else {
                return;
            };
            match graph
                .plan_move(&carried, &selected, self.state.carry_substack)
                .and_then(|plan| {
                    let preview = graph.preview(&plan)?;
                    Ok((plan, preview))
                }) {
                Ok((plan, preview)) => {
                    self.state.pending = Some(plan);
                    self.state.preview = Some(preview);
                    self.update_rendered();
                    self.state.status = None;
                    self.state.status_error = false;
                }
                Err(error) => {
                    self.state.status = Some(error);
                    self.state.status_error = true;
                }
            }
        } else {
            let branch = self
                .state
                .graph
                .as_ref()
                .and_then(|graph| checkout_branch(graph, &selected));
            self.start_operation(
                Operation::Checkout {
                    revision: selected,
                    branch,
                },
                Busy::Mutation,
                "checking out…",
            );
        }
    }

    fn apply(&mut self) {
        let Some(plan) = self.state.pending.clone() else {
            return;
        };
        if self.start_operation(Operation::Apply(plan), Busy::Mutation, "applying…") {
            self.state.pending = None;
        }
    }

    fn refresh(&mut self) {
        if self.state.busy.is_some() {
            self.start_operation(Operation::Refresh, Busy::Load, "refreshing…");
            return;
        }
        self.clear_preview(false);
        self.start_operation(Operation::Refresh, Busy::Load, "refreshing…");
    }

    fn preview_publish(&mut self, remote: String) {
        let mut options = self.publish_options.clone();
        options.remote = remote;
        if self.state.busy.is_some() {
            self.start_operation(
                Operation::PublishPreview(options),
                Busy::Load,
                "planning publish…",
            );
            return;
        }
        self.clear_preview(false);
        self.start_operation(
            Operation::PublishPreview(options),
            Busy::Load,
            "planning publish…",
        );
    }

    fn execute_publish(&mut self) {
        let Some(plan) = self.state.publish_plan.clone() else {
            self.state.status = Some("press p or u to preview publish changes first".into());
            self.state.status_error = true;
            return;
        };
        self.start_operation(
            Operation::PublishExecute(plan),
            Busy::Mutation,
            "publishing and linking…",
        );
    }

    fn update_search(&mut self) {
        let needle = self.state.search.query.to_lowercase();
        self.state.search.matches = self
            .state
            .graph
            .as_ref()
            .map(|graph| {
                graph
                    .order
                    .iter()
                    .filter(|id| {
                        graph.commits.get(*id).is_some_and(|commit| {
                            commit_label(commit, graph.branch.as_deref())
                                .to_lowercase()
                                .contains(&needle)
                        })
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
    }

    fn next_match(&mut self, backwards: bool) {
        if self.state.search.matches.is_empty() {
            return;
        }
        let current = self
            .state
            .selected
            .as_ref()
            .and_then(|id| self.state.search.matches.iter().position(|item| item == id));
        let index = match (current, backwards) {
            (Some(0), true) | (None, true) => self.state.search.matches.len() - 1,
            (Some(index), true) => index - 1,
            (Some(index), false) if index + 1 < self.state.search.matches.len() => index + 1,
            _ => 0,
        };
        self.state.selected = Some(self.state.search.matches[index].clone());
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => self.state.search.editing = false,
            KeyCode::Enter => {
                self.state.search.editing = false;
                self.update_search();
                self.next_match(false);
            }
            KeyCode::Up => self.next_match(true),
            KeyCode::Down => self.next_match(false),
            KeyCode::Backspace => {
                self.state.search.query.pop();
                self.update_search();
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state.search.query.push(character);
                self.update_search();
            }
            _ => return false,
        }
        true
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.help_open {
            self.help_open = help_transition(true, &key).unwrap_or(true);
            return true;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            if self.has_sidebar() {
                self.pane = match self.pane {
                    Pane::Repositories => Pane::Graph,
                    Pane::Graph => Pane::Repositories,
                };
            }
            return true;
        }
        if self.pane == Pane::Repositories && self.repository_search.editing {
            return self.handle_repository_key(key);
        }
        if self.pane == Pane::Graph && self.state.search.editing {
            return self.handle_search_key(key);
        }
        if key.code == KeyCode::Char('?') {
            self.help_open = true;
            return true;
        }
        if self.pane == Pane::Repositories {
            if key.code == KeyCode::Char('q') {
                if self.state.busy == Some(Busy::Mutation) {
                    self.reject_busy_operation();
                } else {
                    self.quit = true;
                }
                return true;
            }
            return self.handle_repository_key(key);
        }
        if self.active.is_none() {
            match key.code {
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Char('r') => {
                    self.rescan();
                }
                _ => return false,
            }
            return true;
        }
        if self.state.busy.is_some()
            && matches!(
                key.code,
                KeyCode::Enter | KeyCode::Char('m' | 'M' | 'p' | 'r' | 'u' | 'q')
            )
        {
            self.reject_busy_operation();
            return true;
        }
        if let Some(substack) = move_pick(&key) {
            self.pick(substack);
            return true;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => self.move_cursor(-1),
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => self.move_cursor(1),
            (KeyCode::Enter, _) => self.enter(),
            (KeyCode::Char('p'), _) => self.preview_publish(self.publish_options.remote.clone()),
            (KeyCode::Char('u'), _) => self.preview_publish("upstream".into()),
            (KeyCode::Esc, _) => self.clear_preview(false),
            (KeyCode::Char('/'), _) => {
                self.clear_preview(true);
                self.state.search.editing = true;
                self.state.search.query.clear();
                self.state.search.matches.clear();
            }
            (KeyCode::Char('n'), modifiers) if modifiers.contains(KeyModifiers::SHIFT) => {
                self.next_match(true)
            }
            (KeyCode::Char('N'), _) => self.next_match(true),
            (KeyCode::Char('n'), _) => self.next_match(false),
            (KeyCode::Char('r'), _) => {
                if self.rescan() && self.active.is_some() {
                    self.refresh();
                }
            }
            (KeyCode::Char('q'), _) => self.quit = true,
            _ => return false,
        }
        true
    }

    fn receive(&mut self, response: OperationResult) {
        if self.active.as_ref() == Some(&response.repo) {
            self.state.receive(response, true);
            self.update_search();
            self.dirty = true;
        } else if let Some(state) = self.cached.get_mut(&response.repo) {
            state.receive(response, false);
        }
    }

    fn render_panes(&mut self, frame: &mut ratatui::Frame, area: Rect) -> Rect {
        let sidebar = self.has_sidebar();
        let (repository_area, graph_area) = if sidebar && area.width >= 80 {
            let [left, right] =
                Layout::horizontal([Constraint::Length(26), Constraint::Min(1)]).areas(area);
            (left, right)
        } else if sidebar && self.pane == Pane::Repositories {
            (area, Rect::default())
        } else {
            (Rect::default(), area)
        };
        if repository_area.width > 0 {
            let title = if self.repository_search.query.is_empty() {
                " Repositories ".to_owned()
            } else {
                format!(" Repositories /{} ", self.repository_search.query)
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(if self.pane == Pane::Repositories {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }));
            let inner = block.inner(repository_area);
            frame.render_widget(block, repository_area);
            let repositories = self.filtered_repositories();
            if self.repository_cursor < self.repository_scroll {
                self.repository_scroll = self.repository_cursor;
            } else if self.repository_cursor
                >= self.repository_scroll.saturating_add(inner.height as usize)
            {
                self.repository_scroll = self
                    .repository_cursor
                    .saturating_sub((inner.height as usize).saturating_sub(1));
            }
            let rows: Vec<Line> = repositories
                .iter()
                .enumerate()
                .skip(self.repository_scroll)
                .take(inner.height as usize)
                .map(|(index, root)| {
                    let active = self.active.as_ref() == Some(root);
                    let selected = index == self.repository_cursor;
                    let name = root
                        .file_name()
                        .unwrap_or(root.as_os_str())
                        .to_string_lossy();
                    let style = Style::default()
                        .fg(if active { Color::Cyan } else { Color::White })
                        .bg(if selected && self.pane == Pane::Repositories {
                            Color::Rgb(64, 64, 64)
                        } else {
                            Color::Reset
                        });
                    Line::styled(
                        format!(
                            "{} {} {}",
                            if selected && self.pane == Pane::Repositories {
                                ">"
                            } else {
                                " "
                            },
                            if active { "●" } else { " " },
                            name
                        ),
                        style,
                    )
                })
                .collect();
            if rows.is_empty() {
                let text = if self.workspace.repositories.is_empty() {
                    "No repositories. Press r to rescan."
                } else {
                    "No matching repositories"
                };
                frame.render_widget(Paragraph::new(text), inner);
            } else {
                frame.render_widget(Paragraph::new(rows), inner);
            }
        }
        if graph_area.width == 0 {
            return graph_area;
        }
        let title = self
            .active
            .as_ref()
            .map(|path| format!(" {} ", path.display()))
            .unwrap_or_else(|| " Graph ".into());
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(if self.pane == Pane::Graph {
                Color::Cyan
            } else {
                Color::DarkGray
            }));
        let inner = block.inner(graph_area);
        frame.render_widget(block, graph_area);
        inner
    }

    fn render_frame(&mut self, frame: &mut ratatui::Frame) -> Vec<TerminalLink> {
        let rendered = self.state.rendered.clone();
        let selected = self.state.selected.clone();
        let carried_commits = self.state.carried_commits.clone();
        let status_error = self.state.status_error;
        let help_open = self.help_open;
        let mut terminal_links = Vec::new();
        let [content_area, message_area, footer_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let graph_area = self.render_panes(frame, content_area);
        let height = graph_area.height as usize;
        let selected_row = rendered
            .iter()
            .position(|line| line.commit.as_ref() == selected.as_ref())
            .unwrap_or(0);
        if height == 0 {
            // Preserve the graph position while only the sidebar is visible.
        } else if selected_row < self.state.scroll {
            self.state.scroll = selected_row;
        } else if selected_row >= self.state.scroll.saturating_add(height) {
            self.state.scroll = selected_row.saturating_sub(height.saturating_sub(1));
        }
        let visible: Vec<RenderedLine> = rendered
            .iter()
            .skip(self.state.scroll)
            .take(height)
            .cloned()
            .collect();
        let lines: Vec<Line> = visible
            .iter()
            .map(|line| {
                styled_graph_line(
                    line,
                    graph_line_style(line, selected.as_ref(), &carried_commits),
                )
            })
            .collect();
        if rendered.is_empty() {
            let placeholder = if self.active.is_none() {
                "Select a repository and press Enter"
            } else if self.state.busy.is_some() {
                "Loading graph…"
            } else if self.state.status_error {
                "Graph unavailable — press r to retry"
            } else {
                "No commits"
            };
            frame.render_widget(
                Paragraph::new(placeholder).style(Style::default().fg(Color::DarkGray)),
                graph_area,
            );
        } else {
            frame.render_widget(Paragraph::new(lines), graph_area);
        }
        for (row, line) in visible.iter().enumerate() {
            let style = graph_line_style(line, selected.as_ref(), &carried_commits);
            for link in &line.links {
                let x = graph_area.x.saturating_add(link.start as u16);
                let y = graph_area.y.saturating_add(row as u16);
                if x >= graph_area.right() || y >= graph_area.bottom() {
                    continue;
                }
                let visible_width = link.width.min((graph_area.right() - x) as usize);
                terminal_links.push(TerminalLink {
                    x,
                    y,
                    text: link.text.chars().take(visible_width).collect(),
                    url: link.url.clone(),
                    style: text_kind_style(style, TextKind::RemoteRef),
                });
            }
        }

        let focused_search = if self.pane == Pane::Repositories {
            &self.repository_search
        } else {
            &self.state.search
        };
        let message = if focused_search.editing {
            Line::from(vec![
                Span::styled("/", Style::default().fg(Color::Cyan)),
                Span::raw(&focused_search.query),
            ])
        } else if let Some(status) = self.state.status.as_deref() {
            let prefix = if status_error { "Error: " } else { "" };
            Line::from(format!(" {prefix}{status}"))
        } else {
            Line::default()
        };
        let message_style = if status_error && !focused_search.editing {
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan).bg(Color::Rgb(40, 40, 40))
        };
        frame.render_widget(Paragraph::new(message).style(message_style), message_area);

        let footer = if frame.area().width < 80 {
            key_line(&[
                ("Tab", "pane"),
                ("/", "search"),
                ("Enter", "open"),
                ("?", "help"),
                ("q", "quit"),
            ])
        } else if self.pane == Pane::Repositories {
            key_line(&[
                ("Tab/⇧Tab", "pane"),
                ("↑/↓", "select"),
                ("Enter", "activate"),
                ("/", "filter"),
                ("?", "help"),
                ("q", "quit"),
            ])
        } else {
            compact_footer()
        };
        frame.render_widget(Paragraph::new(footer), footer_area);

        if help_open {
            let area = centered(frame.area(), 68, 16);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(help_lines()).block(
                    Block::default()
                        .title(" Help ")
                        .borders(Borders::ALL)
                        .style(Style::default().bg(Color::Rgb(24, 24, 24))),
                ),
                area,
            );
        }
        if help_open {
            terminal_links.clear();
        }
        terminal_links
    }

    fn draw(&mut self, terminal: &mut Tui) -> io::Result<()> {
        let mut links = Vec::new();
        terminal.draw(|frame| links = self.render_frame(frame))?;
        write_terminal_links(terminal.backend_mut(), &links)
    }

    pub fn run(mut self) -> Result<(), String> {
        enable_raw_mode().map_err(|error| error.to_string())?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).map_err(|error| error.to_string())?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend).map_err(|error| error.to_string())?;
        let input_worker = event::start_input(
            self.input_events
                .take()
                .ok_or("terminal input is already running")?,
        );
        let result = (|| -> Result<(), String> {
            while !self.quit {
                if self.dirty {
                    self.draw(&mut terminal)
                        .map_err(|error| error.to_string())?;
                    self.dirty = false;
                }
                match self.events.recv().map_err(|error| error.to_string())? {
                    UiEvent::Terminal(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        if self.handle_key(key) {
                            self.dirty = true;
                        }
                    }
                    UiEvent::Terminal(Event::Resize(_, _)) => self.dirty = true,
                    UiEvent::Terminal(_) => {}
                    UiEvent::OperationCompleted(response) => self.receive(*response),
                    UiEvent::InputError(error) => return Err(error),
                }
            }
            Ok(())
        })();
        let _ = self.operations.send(OperationRequest {
            options: self.publish_options.clone(),
            id: 0,
            operation: Operation::Stop,
        });
        drop(input_worker);
        let _ = disable_raw_mode();
        let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
        let _ = terminal.show_cursor();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver, Sender};

    fn test_app() -> (App, Receiver<OperationRequest>, Sender<UiEvent>) {
        let (operation_tx, operation_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let mut app = App::with_workspace(
            SubmitOptions::default(),
            Workspace {
                root: PathBuf::from("."),
                repositories: vec![PathBuf::from(".")],
                initial: None,
            },
            event::EventLoop {
                operations: operation_tx,
                events: event_rx,
                input_events: event_tx.clone(),
            },
        );
        app.active = Some(PathBuf::from("."));
        app.pane = Pane::Graph;
        (app, operation_rx, event_tx)
    }

    fn graph_response() -> OperationResult {
        OperationResult {
            repo: PathBuf::from("."),
            id: 0,
            graph: Ok(Graph::default()),
            preview: None,
            publish_plan: None,
            pr_links: None,
            operation_error: None,
        }
    }

    fn workspace_app() -> (App, Receiver<OperationRequest>) {
        let (mut app, requests, _) = test_app();
        app.active = None;
        app.pane = Pane::Repositories;
        app.workspace = Workspace {
            root: PathBuf::from("/projects"),
            repositories: vec![
                PathBuf::from("/projects/alpha"),
                PathBuf::from("/projects/jupiter"),
            ],
            initial: None,
        };
        (app, requests)
    }

    fn key(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn loaded(request: &OperationRequest) -> OperationResult {
        let mut response = graph_response();
        response.repo = request.options.repo.clone();
        response.id = request.id;
        response.graph = Ok(Graph {
            commits: ["a", "b"]
                .into_iter()
                .map(|id| {
                    (
                        id.into(),
                        crate::ui::model::Commit {
                            id: id.into(),
                            subject: format!("commit {id}"),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            order: vec!["a".into(), "b".into()],
            ..Default::default()
        });
        response
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                app.render_frame(frame);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn browsing_filtering_and_pane_focus_never_load_history() {
        let (mut app, requests) = workspace_app();
        assert!(screen(&mut app, 100, 15).contains("Select a repository"));
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Tab);
        assert!(app.pane == Pane::Graph);
        key(&mut app, KeyCode::BackTab);
        key(&mut app, KeyCode::Char('/'));
        key(&mut app, KeyCode::Char('j'));
        assert_eq!(app.repository_search.query, "j");
        assert_eq!(
            app.filtered_repositories(),
            vec![PathBuf::from("/projects/jupiter")]
        );
        key(&mut app, KeyCode::Enter); // Finish typing, then explicitly activate.
        assert!(requests.try_recv().is_err());
        assert!(app.active.is_none());
        key(&mut app, KeyCode::Enter);
        let request = requests.try_recv().unwrap();
        assert!(matches!(request.operation, Operation::Load));
        assert_eq!(request.options.repo, PathBuf::from("/projects/jupiter"));
        assert!(app.pane == Pane::Repositories);
    }

    #[test]
    fn switching_restores_graph_selection_scroll_and_independent_searches() {
        let (mut app, requests) = workspace_app();
        key(&mut app, KeyCode::Enter);
        let alpha = requests.try_recv().unwrap();
        app.receive(loaded(&alpha));
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Char('/'));
        key(&mut app, KeyCode::Char('b'));
        key(&mut app, KeyCode::Down);
        assert_eq!(app.state.selected.as_deref(), Some("b"));
        key(&mut app, KeyCode::Esc);
        app.state.scroll = 9;
        let rendered = app.state.rendered.clone();
        key(&mut app, KeyCode::BackTab);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        let jupiter = requests.try_recv().unwrap();
        assert!(app.state.search.query.is_empty());
        app.receive(loaded(&jupiter));
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Enter);
        assert!(requests.try_recv().is_err());
        assert_eq!(app.state.scroll, 9);
        assert_eq!(app.state.selected.as_deref(), Some("b"));
        assert_eq!(app.state.search.query, "b");
        assert!(app.repository_search.query.is_empty());
        assert_eq!(
            app.state
                .rendered
                .iter()
                .map(|r| &r.text)
                .collect::<Vec<_>>(),
            rendered.iter().map(|r| &r.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn late_loads_are_cached_without_rendering_or_replacing_the_active_graph() {
        let (mut app, requests) = workspace_app();
        key(&mut app, KeyCode::Enter);
        let alpha = requests.try_recv().unwrap();
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        let jupiter = requests.try_recv().unwrap();
        app.receive(loaded(&alpha));
        assert!(app.state.graph.is_none());
        assert!(app.cached[&alpha.options.repo].graph.is_some());
        assert!(app.cached[&alpha.options.repo].rendered.is_empty());
        assert_eq!(app.state.busy, Some(Busy::Load));
        app.receive(loaded(&jupiter));
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Enter);
        assert!(!app.state.rendered.is_empty());
        assert!(requests.try_recv().is_err());
        // A response from an older request cannot clear a newer load or replace state.
        app.start_operation(Operation::Load, Busy::Load, "loading");
        let newer = requests.try_recv().unwrap();
        let mut stale = loaded(&alpha);
        stale.graph = Err("old error".into());
        app.receive(stale);
        assert_eq!(app.state.request_id, newer.id);
        assert_eq!(app.state.busy, Some(Busy::Load));
        assert!(!app.state.status_error);
    }

    #[test]
    fn switching_cancels_previews_and_mutations_block_activation() {
        let (mut app, requests) = workspace_app();
        key(&mut app, KeyCode::Enter);
        let alpha = requests.try_recv().unwrap();
        app.receive(loaded(&alpha));
        app.state.preview = Some(Graph::default());
        app.state.carried = Some("a".into());
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        let jupiter = requests.try_recv().unwrap();
        assert!(app.cached[&alpha.options.repo].preview.is_none());
        assert!(app.cached[&alpha.options.repo].carried.is_none());
        app.receive(loaded(&jupiter));
        app.state.busy = Some(Busy::Mutation);
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.active.as_ref(), Some(&jupiter.options.repo));
        assert!(app.state.status_error);
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn cancelled_in_flight_preview_does_not_reappear_after_returning() {
        let (mut app, requests) = workspace_app();
        key(&mut app, KeyCode::Enter);
        let alpha = requests.try_recv().unwrap();
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        let _jupiter = requests.try_recv().unwrap();
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Enter);
        let mut response = loaded(&alpha);
        response.preview = Some(Graph::default());
        app.receive(response);
        assert!(app.state.preview.is_none());
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn narrow_terminal_shows_focused_pane_and_preserves_hidden_graph_scroll() {
        let (mut app, requests) = workspace_app();
        key(&mut app, KeyCode::Enter);
        let alpha = requests.try_recv().unwrap();
        app.receive(loaded(&alpha));
        let wide = screen(&mut app, 100, 15);
        assert!(wide.contains("● alpha"));
        assert!(wide.contains("commit a"));
        key(&mut app, KeyCode::Down);
        assert_eq!(app.active.as_ref(), Some(&alpha.options.repo));
        app.state.scroll = 9;
        let narrow = screen(&mut app, 50, 10);
        assert!(narrow.contains("Repositories"));
        assert!(!narrow.contains("commit a"));
        assert_eq!(app.state.scroll, 9);
        key(&mut app, KeyCode::Tab);
        let graph = screen(&mut app, 50, 10);
        assert!(!graph.contains("Repositories"));
        assert!(graph.contains("commit a"));
        screen(&mut app, 1, 1); // Tiny resizes must not panic.
    }

    #[test]
    fn move_bindings_use_lowercase_for_commit_and_uppercase_for_substack() {
        assert_eq!(
            move_pick(&KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
            Some(false)
        );
        assert_eq!(
            move_pick(&KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT)),
            Some(true)
        );
        assert_eq!(
            move_pick(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            move_pick(&KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn navigation_preserves_only_publish_previews() {
        assert!(preserve_preview_while_navigating(true));
        assert!(!preserve_preview_while_navigating(false));
    }

    #[test]
    fn checkout_uses_a_unique_displayed_local_branch() {
        let graph = Graph {
            commits: [
                (
                    "one".into(),
                    crate::ui::model::Commit {
                        local_refs: vec!["fs-head/topic/1".into()],
                        ..crate::ui::model::Commit::default()
                    },
                ),
                (
                    "many".into(),
                    crate::ui::model::Commit {
                        local_refs: vec!["main".into(), "fs-head/topic/1".into()],
                        ..crate::ui::model::Commit::default()
                    },
                ),
            ]
            .into(),
            ..Graph::default()
        };

        assert_eq!(
            checkout_branch(&graph, "one").as_deref(),
            Some("fs-head/topic/1")
        );
        assert_eq!(checkout_branch(&graph, "many"), None);
        assert_eq!(checkout_branch(&graph, "missing"), None);
    }

    #[test]
    fn help_is_modal_and_toggles_with_question_mark_or_escape() {
        let question = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let action = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE);

        assert_eq!(help_transition(false, &question), Some(true));
        assert_eq!(help_transition(true, &action), Some(true));
        assert_eq!(help_transition(true, &question), Some(false));
        assert_eq!(help_transition(true, &escape), Some(false));
        assert_eq!(help_transition(false, &action), None);
    }

    #[test]
    fn footer_is_compact_and_full_key_map_lives_in_help() {
        let footer = compact_footer().to_string();
        assert!(footer.contains("Enter open/confirm"));
        assert!(footer.contains("? help"));
        assert!(footer.contains("q quit"));
        assert!(!footer.contains("move"));
        assert!(!footer.contains("publish"));
        assert!(!footer.contains("search"));

        let help = help_lines()
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        assert_eq!(help.len(), 13);
        for binding in [
            "m/M          move commit / substack",
            "p            preview publish and stack link",
            "u            preview upstream publish and stack link",
            "/            search focused pane",
            "r            rescan; in graph also refresh graph/PRs",
        ] {
            assert!(
                help.iter().any(|line| line == binding),
                "missing aligned line {binding:?} from {help:?}"
            );
        }
    }

    #[test]
    fn publish_keys_select_the_configured_and_upstream_remotes() {
        let (mut origin_app, origin_operations, _events) = test_app();
        origin_app.publish_options.remote = "origin".into();
        origin_app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(matches!(
            origin_operations.recv().unwrap().operation,
            Operation::PublishPreview(options) if options.remote == "origin"
        ));

        let (mut upstream_app, upstream_operations, _events) = test_app();
        upstream_app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
        assert!(matches!(
            upstream_operations.recv().unwrap().operation,
            Operation::PublishPreview(options) if options.remote == "upstream"
        ));
    }

    #[test]
    fn enter_confirms_move_and_publish_previews() {
        let (mut move_app, move_operations, _events) = test_app();
        move_app.state.pending = Some(MovePlan {
            selected: "selected".into(),
            destination: "destination".into(),
            include_descendants: false,
            base: "base".into(),
            source_base: "source-base".into(),
            carried_count: 1,
            tip: "tip".into(),
            tip_commit: "tip-commit".into(),
            detach_for_rewrite: false,
            checkout_branch: "branch".into(),
            ref_updates: Vec::new(),
            commits: vec!["selected".into()],
        });
        move_app.enter();
        assert!(matches!(
            move_operations.recv().unwrap().operation,
            Operation::Apply(_)
        ));

        let (mut publish_app, publish_operations, _events) = test_app();
        publish_app.state.publish_plan = Some(SubmitPlan {
            options: SubmitOptions::default(),
            fork: "owner/repo".into(),
            base_ref: "origin/main".into(),
            commits: Vec::new(),
            updates: Vec::new(),
        });
        publish_app.enter();
        assert!(matches!(
            publish_operations.recv().unwrap().operation,
            Operation::PublishExecute(_)
        ));
    }

    #[test]
    fn removed_stack_and_uppercase_keys_do_nothing() {
        let (mut app, operations, _events) = test_app();
        for key in ['a', 's', 'P', 'R'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
        assert!(matches!(
            operations.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        app.refresh();
        assert!(matches!(
            operations.recv().unwrap().operation,
            Operation::Refresh
        ));
    }

    #[test]
    fn git_decoration_colors_match_the_requested_palette() {
        let base = Style::default().bg(Color::Rgb(64, 64, 64));

        assert_eq!(
            text_kind_style(base, TextKind::CommitHash).fg,
            Some(Color::Yellow)
        );
        let head = text_kind_style(base, TextKind::Head);
        assert_eq!(head.fg, Some(Color::Cyan));
        assert!(head.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            text_kind_style(base, TextKind::LocalRef).fg,
            Some(Color::Green)
        );
        assert_eq!(
            text_kind_style(base, TextKind::RemoteRef).fg,
            Some(Color::Red)
        );
        assert_eq!(text_kind_style(base, TextKind::Tag).fg, Some(Color::Yellow));
        assert_eq!(head.bg, base.bg);
    }

    #[test]
    fn osc_hyperlink_representation_rejects_control_characters() {
        assert_eq!(
            hyperlink_sequence("https://example.invalid/1", "origin/fs-head/topic/1"),
            Some("\x1b]8;;https://example.invalid/1\x07origin/fs-head/topic/1\x1b]8;;\x07".into())
        );
        assert!(hyperlink_sequence("https://example.invalid/\x1b", "origin").is_none());
        assert!(hyperlink_sequence("https://example.invalid/1", "ori\ngin").is_none());
    }

    #[test]
    fn terminal_hyperlink_wraps_the_whole_ref_once() {
        let mut output = Vec::new();
        write_terminal_links(
            &mut output,
            &[TerminalLink {
                x: 2,
                y: 3,
                text: "origin/fs-head/topic/1".into(),
                url: "https://example.invalid/1".into(),
                style: Style::default(),
            }],
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(
            output.contains(
                "\x1b]8;;https://example.invalid/1\x07origin/fs-head/topic/1\x1b]8;;\x07"
            )
        );
        assert_eq!(
            output.matches("\x1b]8;;https://example.invalid/1").count(),
            1
        );
        assert!(!output.contains("\x1b[4m"));
    }

    #[test]
    fn graph_load_does_not_fetch_pr_links_without_explicit_refresh() {
        let (mut app, _operations, _events) = test_app();
        app.receive(graph_response());
        assert!(app.state.pr_links.is_empty());
    }

    #[test]
    fn explicit_refresh_is_one_combined_operation() {
        let (mut app, operations, _events) = test_app();
        app.refresh();
        assert!(matches!(
            operations.recv().unwrap().operation,
            Operation::Refresh
        ));
        assert!(matches!(
            operations.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn normal_graph_updates_keep_existing_pr_links_without_refetching() {
        let (mut app, operations, _events) = test_app();
        app.state.pr_links.insert(
            "origin/fs-head/topic/1".into(),
            PullRequestLink {
                number: 1,
                head_ref_name: "fs-head/topic/1".into(),
                url: "https://example.invalid/1".into(),
            },
        );
        app.receive(graph_response());

        assert_eq!(app.state.pr_links.len(), 1);
        assert!(matches!(
            operations.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn successful_publish_updates_pr_links_without_refetching() {
        let (mut app, operations, _events) = test_app();
        let link = PullRequestLink {
            number: 1,
            head_ref_name: "fs-head/topic/1".into(),
            url: "https://example.invalid/1".into(),
        };
        app.receive(OperationResult {
            pr_links: Some(Ok([("origin/fs-head/topic/1".into(), link.clone())].into())),
            ..graph_response()
        });

        assert_eq!(
            app.state.pr_links.get("origin/fs-head/topic/1"),
            Some(&link)
        );
        assert!(matches!(
            operations.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn navigation_and_help_remain_available_while_an_operation_runs() {
        let (mut app, _operations, _events) = test_app();
        let graph = Graph {
            commits: [
                (
                    "a".into(),
                    crate::ui::model::Commit {
                        id: "a".into(),
                        ..crate::ui::model::Commit::default()
                    },
                ),
                (
                    "b".into(),
                    crate::ui::model::Commit {
                        id: "b".into(),
                        ..crate::ui::model::Commit::default()
                    },
                ),
            ]
            .into(),
            order: vec!["a".into(), "b".into()],
            ..Graph::default()
        };
        app.state.graph = Some(graph);
        app.state.selected = Some("a".into());
        app.state.busy = Some(Busy::Load);

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.state.selected.as_deref(), Some("b"));
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(app.help_open);
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(app.state.search.editing);
    }

    #[test]
    fn workflow_and_quit_keys_are_rejected_while_an_operation_runs() {
        for key in [
            KeyCode::Char('m'),
            KeyCode::Char('M'),
            KeyCode::Enter,
            KeyCode::Char('p'),
            KeyCode::Char('r'),
            KeyCode::Char('u'),
            KeyCode::Char('q'),
        ] {
            let (mut app, operations, _events) = test_app();
            app.state.busy = Some(Busy::Load);
            app.state.selected = Some("selected".into());
            app.state.carried = (key == KeyCode::Enter).then(|| "carried".into());

            app.handle_key(KeyEvent::new(key, KeyModifiers::NONE));

            assert_eq!(
                app.state.status.as_deref(),
                Some("another operation is already running")
            );
            assert!(app.state.status_error);
            assert!(app.state.pending.is_none());
            assert!(app.state.preview.is_none());
            assert!(!app.quit);
            assert!(matches!(
                operations.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
        }
    }

    #[test]
    fn a_second_operation_is_rejected_with_a_visible_error() {
        let (mut app, operations, _events) = test_app();
        assert!(app.start_operation(Operation::Load, Busy::Load, "loading…"));

        app.refresh();

        assert_eq!(
            app.state.status.as_deref(),
            Some("another operation is already running")
        );
        assert!(app.state.status_error);
        assert!(matches!(
            operations.recv().unwrap().operation,
            Operation::Load
        ));
        assert!(matches!(
            operations.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }
}
