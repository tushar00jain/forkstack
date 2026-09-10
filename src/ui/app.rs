use std::collections::{BTreeMap, HashSet};
use std::io::{self, Stdout, Write};
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
use crate::ui::event::{self, Operation, OperationResult, UiEvent};
use crate::ui::model::{Graph, MovePlan};
use crate::ui::render::{RenderedLine, TextKind, attach_pr_links, commit_label, render_graph};

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
        help_line("↑/↓, j/k", "select previous/next commit"),
        help_line("Enter", "checkout or confirm preview"),
        help_line("m/M", "move commit / substack"),
        help_line("Esc", "cancel preview or close help"),
        help_line("p", "preview publish"),
        help_line("/", "search"),
        help_line("n/N", "next/previous match"),
        help_line("r", "refresh graph and PR links"),
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

pub struct App {
    graph: Option<Graph>,
    preview: Option<Graph>,
    selected: Option<String>,
    carried: Option<String>,
    carried_commits: HashSet<String>,
    carry_substack: bool,
    pending: Option<MovePlan>,
    publish_options: SubmitOptions,
    publish_plan: Option<SubmitPlan>,
    search: Search,
    status: Option<String>,
    status_error: bool,
    busy: Option<Busy>,
    operations: Sender<Operation>,
    events: Receiver<UiEvent>,
    input_events: Option<Sender<UiEvent>>,
    quit: bool,
    scroll: usize,
    rendered: Vec<RenderedLine>,
    pr_links: BTreeMap<String, PullRequestLink>,
    help_open: bool,
    dirty: bool,
}

impl App {
    pub fn new(publish_options: SubmitOptions) -> Self {
        let event_loop = event::start(publish_options.clone());
        let app = Self {
            graph: None,
            preview: None,
            selected: None,
            carried: None,
            carried_commits: HashSet::new(),
            carry_substack: false,
            pending: None,
            publish_options,
            publish_plan: None,
            search: Search::default(),
            status: None,
            status_error: false,
            busy: Some(Busy::Load),
            operations: event_loop.operations,
            events: event_loop.events,
            input_events: Some(event_loop.input_events),
            quit: false,
            scroll: 0,
            rendered: Vec::new(),
            pr_links: BTreeMap::new(),
            help_open: false,
            dirty: true,
        };
        let _ = app.operations.send(Operation::Load);
        app
    }

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
        self.dirty = true;
    }

    fn clear_preview(&mut self, keep_carried: bool) {
        let had_preview = self.preview.take().is_some();
        self.pending = None;
        self.publish_plan = None;
        if !keep_carried {
            self.carried = None;
            self.carried_commits.clear();
        }
        if had_preview {
            self.update_rendered();
        }
    }

    fn start_operation(&mut self, operation: Operation, busy: Busy, status: &str) -> bool {
        if self.busy.is_some() {
            self.reject_busy_operation();
            return false;
        }
        if self.operations.send(operation).is_err() {
            self.status = Some("operation worker is unavailable".into());
            self.status_error = true;
            self.dirty = true;
            return false;
        }
        self.busy = Some(busy);
        self.status = Some(status.into());
        self.status_error = false;
        self.dirty = true;
        true
    }

    fn reject_busy_operation(&mut self) {
        self.status = Some("another operation is already running".into());
        self.status_error = true;
        self.dirty = true;
    }

    fn move_cursor(&mut self, amount: isize) {
        if !preserve_preview_while_navigating(self.publish_plan.is_some()) {
            self.clear_preview(true);
        }
        let ids = self.graph_ids();
        if ids.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|id| ids.iter().position(|item| item == id))
            .unwrap_or(0);
        let next = (current as isize + amount).clamp(0, ids.len() as isize - 1) as usize;
        self.selected = Some(ids[next].clone());
    }

    fn pick(&mut self, substack: bool) {
        let Some(selected) = self.selected.clone() else {
            return;
        };
        let commits = if substack {
            let Some(graph) = self.graph.as_ref() else {
                return;
            };
            match graph.carried_substack(&selected) {
                Ok(commits) => commits,
                Err(error) => {
                    self.clear_preview(false);
                    self.status = Some(error);
                    self.status_error = true;
                    return;
                }
            }
        } else {
            vec![selected.clone()]
        };
        self.clear_preview(false);
        self.carried = Some(selected);
        self.carried_commits = commits.into_iter().collect();
        self.carry_substack = substack;
        self.status = None;
        self.status_error = false;
    }

    fn enter(&mut self) {
        if self.pending.is_some() {
            self.apply();
            return;
        }
        if self.publish_plan.is_some() {
            self.execute_publish();
            return;
        }
        let Some(selected) = self.selected.clone() else {
            return;
        };
        if let Some(carried) = self.carried.clone() {
            let Some(graph) = self.graph.as_ref() else {
                return;
            };
            match graph
                .plan_move(&carried, &selected, self.carry_substack)
                .and_then(|plan| {
                    let preview = graph.preview(&plan)?;
                    Ok((plan, preview))
                }) {
                Ok((plan, preview)) => {
                    self.pending = Some(plan);
                    self.preview = Some(preview);
                    self.update_rendered();
                    self.status = None;
                    self.status_error = false;
                }
                Err(error) => {
                    self.status = Some(error);
                    self.status_error = true;
                }
            }
        } else {
            let branch = self
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
        let Some(plan) = self.pending.clone() else {
            return;
        };
        if self.start_operation(Operation::Apply(plan), Busy::Mutation, "applying…") {
            self.pending = None;
        }
    }

    fn refresh(&mut self) {
        if self.busy.is_some() {
            self.start_operation(Operation::Refresh, Busy::Load, "refreshing…");
            return;
        }
        self.clear_preview(false);
        self.start_operation(Operation::Refresh, Busy::Load, "refreshing…");
    }

    fn preview_publish(&mut self) {
        if self.busy.is_some() {
            self.start_operation(
                Operation::PublishPreview(self.publish_options.clone()),
                Busy::Load,
                "planning publish…",
            );
            return;
        }
        self.clear_preview(false);
        self.start_operation(
            Operation::PublishPreview(self.publish_options.clone()),
            Busy::Load,
            "planning publish…",
        );
    }

    fn execute_publish(&mut self) {
        let Some(plan) = self.publish_plan.clone() else {
            self.status = Some("press p to preview publish changes first".into());
            self.status_error = true;
            return;
        };
        self.start_operation(
            Operation::PublishExecute(plan),
            Busy::Mutation,
            "publishing…",
        );
    }

    fn update_search(&mut self) {
        let needle = self.search.query.to_lowercase();
        self.search.matches = self
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
        if self.search.matches.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|id| self.search.matches.iter().position(|item| item == id));
        let index = match (current, backwards) {
            (Some(0), true) | (None, true) => self.search.matches.len() - 1,
            (Some(index), true) => index - 1,
            (Some(index), false) if index + 1 < self.search.matches.len() => index + 1,
            _ => 0,
        };
        self.selected = Some(self.search.matches[index].clone());
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => self.search.editing = false,
            KeyCode::Enter => {
                self.search.editing = false;
                self.update_search();
                self.next_match(false);
            }
            KeyCode::Backspace => {
                self.search.query.pop();
                self.update_search();
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search.query.push(character);
                self.update_search();
            }
            _ => return false,
        }
        true
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if let Some(open) = help_transition(self.help_open, &key) {
            let changed = self.help_open != open;
            self.help_open = open;
            return changed;
        }
        if self.search.editing {
            return self.handle_search_key(key);
        }
        if self.busy.is_some()
            && matches!(
                key.code,
                KeyCode::Enter | KeyCode::Char('m' | 'M' | 'p' | 'r' | 'q')
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
            (KeyCode::Char('p'), _) => self.preview_publish(),
            (KeyCode::Esc, _) => self.clear_preview(false),
            (KeyCode::Char('/'), _) => {
                self.clear_preview(true);
                self.search.editing = true;
                self.search.query.clear();
                self.search.matches.clear();
            }
            (KeyCode::Char('n'), modifiers) if modifiers.contains(KeyModifiers::SHIFT) => {
                self.next_match(true)
            }
            (KeyCode::Char('N'), _) => self.next_match(true),
            (KeyCode::Char('n'), _) => self.next_match(false),
            (KeyCode::Char('r'), _) => self.refresh(),
            (KeyCode::Char('q'), _) => self.quit = true,
            _ => return false,
        }
        true
    }

    fn receive(&mut self, response: OperationResult) {
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
        self.update_rendered();
    }

    fn draw(&mut self, terminal: &mut Tui) -> io::Result<()> {
        let rendered = self.rendered.clone();
        let selected = self.selected.clone();
        let carried_commits = self.carried_commits.clone();
        let status_error = self.status_error;
        let help_open = self.help_open;
        let mut terminal_links = Vec::new();
        terminal.draw(|frame| {
            let [graph_area, message_area, footer_area] = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .areas(frame.area());
            let height = graph_area.height as usize;
            let selected_row = rendered
                .iter()
                .position(|line| line.commit.as_ref() == selected.as_ref())
                .unwrap_or(0);
            if selected_row < self.scroll {
                self.scroll = selected_row;
            } else if selected_row >= self.scroll.saturating_add(height) {
                self.scroll = selected_row.saturating_sub(height.saturating_sub(1));
            }
            let visible: Vec<RenderedLine> = rendered
                .iter()
                .skip(self.scroll)
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
            frame.render_widget(Paragraph::new(lines).block(Block::default()), graph_area);
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

            let message = if self.search.editing {
                Line::from(vec![
                    Span::styled("/", Style::default().fg(Color::Cyan)),
                    Span::raw(&self.search.query),
                ])
            } else if let Some(status) = self.status.as_deref() {
                let prefix = status_error.then_some("Error: ").unwrap_or_default();
                Line::from(format!(" {prefix}{status}"))
            } else {
                Line::default()
            };
            let message_style = if status_error && !self.search.editing {
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan).bg(Color::Rgb(40, 40, 40))
            };
            frame.render_widget(Paragraph::new(message).style(message_style), message_area);

            frame.render_widget(Paragraph::new(compact_footer()), footer_area);

            if help_open {
                let area = centered(frame.area(), 62, 12);
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
        })?;
        write_terminal_links(terminal.backend_mut(), &terminal_links)
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
                    UiEvent::OperationCompleted(response) => self.receive(response),
                    UiEvent::InputError(error) => return Err(error),
                }
            }
            Ok(())
        })();
        let _ = self.operations.send(Operation::Stop);
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

    fn test_app() -> (App, Receiver<Operation>, Sender<UiEvent>) {
        let (operation_tx, operation_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let app = App {
            graph: None,
            preview: None,
            selected: None,
            carried: None,
            carried_commits: HashSet::new(),
            carry_substack: false,
            pending: None,
            publish_options: SubmitOptions::default(),
            publish_plan: None,
            search: Search::default(),
            status: None,
            status_error: false,
            busy: None,
            operations: operation_tx,
            events: event_rx,
            input_events: None,
            quit: false,
            scroll: 0,
            rendered: Vec::new(),
            pr_links: BTreeMap::new(),
            help_open: false,
            dirty: false,
        };
        (app, operation_rx, event_tx)
    }

    fn graph_response() -> OperationResult {
        OperationResult {
            graph: Ok(Graph::default()),
            preview: None,
            publish_plan: None,
            pr_links: None,
            operation_error: None,
        }
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
        assert_eq!(help.len(), 10);
        for binding in [
            "m/M          move commit / substack",
            "p            preview publish",
            "/            search",
            "r            refresh graph and PR links",
        ] {
            assert!(
                help.iter().any(|line| line == binding),
                "missing aligned line {binding:?} from {help:?}"
            );
        }
    }

    #[test]
    fn enter_confirms_move_and_publish_previews() {
        let (mut move_app, move_operations, _events) = test_app();
        move_app.pending = Some(MovePlan {
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
            move_operations.recv().unwrap(),
            Operation::Apply(_)
        ));

        let (mut publish_app, publish_operations, _events) = test_app();
        publish_app.publish_plan = Some(SubmitPlan {
            options: SubmitOptions::default(),
            fork: "owner/repo".into(),
            base_ref: "origin/main".into(),
            commits: Vec::new(),
            updates: Vec::new(),
        });
        publish_app.enter();
        assert!(matches!(
            publish_operations.recv().unwrap(),
            Operation::PublishExecute(_)
        ));
    }

    #[test]
    fn removed_uppercase_and_apply_keys_do_nothing() {
        let (mut app, operations, _events) = test_app();
        for key in ['a', 'P', 'R'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
        assert!(matches!(
            operations.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(operations.recv().unwrap(), Operation::Refresh));
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
        assert!(app.pr_links.is_empty());
    }

    #[test]
    fn explicit_refresh_is_one_combined_operation() {
        let (mut app, operations, _events) = test_app();
        app.refresh();
        assert!(matches!(operations.recv().unwrap(), Operation::Refresh));
        assert!(matches!(
            operations.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn normal_graph_updates_keep_existing_pr_links_without_refetching() {
        let (mut app, operations, _events) = test_app();
        app.pr_links.insert(
            "origin/fs-head/topic/1".into(),
            PullRequestLink {
                number: 1,
                head_ref_name: "fs-head/topic/1".into(),
                url: "https://example.invalid/1".into(),
            },
        );
        app.receive(graph_response());

        assert_eq!(app.pr_links.len(), 1);
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

        assert_eq!(app.pr_links.get("origin/fs-head/topic/1"), Some(&link));
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
        app.graph = Some(graph);
        app.selected = Some("a".into());
        app.busy = Some(Busy::Load);

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected.as_deref(), Some("b"));
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(app.help_open);
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(app.search.editing);
    }

    #[test]
    fn workflow_and_quit_keys_are_rejected_while_an_operation_runs() {
        for key in [
            KeyCode::Char('m'),
            KeyCode::Char('M'),
            KeyCode::Enter,
            KeyCode::Char('p'),
            KeyCode::Char('r'),
            KeyCode::Char('q'),
        ] {
            let (mut app, operations, _events) = test_app();
            app.busy = Some(Busy::Load);
            app.selected = Some("selected".into());
            app.carried = (key == KeyCode::Enter).then(|| "carried".into());

            app.handle_key(KeyEvent::new(key, KeyModifiers::NONE));

            assert_eq!(
                app.status.as_deref(),
                Some("another operation is already running")
            );
            assert!(app.status_error);
            assert!(app.pending.is_none());
            assert!(app.preview.is_none());
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
            app.status.as_deref(),
            Some("another operation is already running")
        );
        assert!(app.status_error);
        assert!(matches!(operations.recv().unwrap(), Operation::Load));
        assert!(matches!(
            operations.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }
}
