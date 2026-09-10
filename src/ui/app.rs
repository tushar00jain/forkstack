use std::collections::{BTreeMap, HashSet};
use std::io::{self, Stdout, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use crossterm::cursor::MoveTo;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{
    Attribute as TerminalAttribute, Color as TerminalColor, Colors, Print, SetAttribute, SetColors,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::core::submit::{SubmitOptions, SubmitPlan};
use crate::integrations::github::PullRequestLink;
use crate::ui::git::{
    LinkRequest, LinkResponse, Request, Response, displayed_remote_heads, link_worker, worker,
};
use crate::ui::model::{Graph, MovePlan};
use crate::ui::render::{RenderedLine, attach_pr_links, commit_label, render_graph};

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

fn help_transition(open: bool, key: &KeyEvent) -> Option<bool> {
    if open {
        Some(!matches!(key.code, KeyCode::Char('?') | KeyCode::Esc))
    } else {
        (key.code == KeyCode::Char('?')).then_some(true)
    }
}

fn current_link_response(generation: u64, response: &LinkResponse) -> bool {
    response.generation == generation
}

fn link_request(generation: u64, graph: &Graph, remote: &str) -> LinkRequest {
    LinkRequest::Load {
        generation,
        heads: displayed_remote_heads(graph, remote),
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
        ("Enter", "open"),
        ("m/M", "move"),
        ("a", "apply"),
        ("p/P", "publish"),
        ("?", "help"),
        ("q", "quit"),
    ])
}

fn help_lines() -> Vec<Line<'static>> {
    vec![
        key_line(&[("↑/↓ or j/k", "select previous/next commit")]),
        key_line(&[("Enter", "checkout commit or preview pending move")]),
        key_line(&[("m", "move commit"), ("M", "move substack")]),
        key_line(&[("a", "apply move preview"), ("Esc", "cancel preview")]),
        key_line(&[("p", "preview publish"), ("P", "execute publish")]),
        key_line(&[("/", "search"), ("n/N", "next/previous match")]),
        key_line(&[("r/R", "refresh graph and PR links")]),
        key_line(&[("? or Esc", "close help"), ("q", "quit")]),
    ]
}

fn hyperlink_sequence(url: &str, text: &str) -> Option<String> {
    (!url.chars().any(char::is_control) && !text.chars().any(char::is_control))
        .then(|| format!("\x1B]8;;{url}\x07{text}\x1B]8;;\x07"))
}

fn underline_links(buffer: &mut Buffer, area: Rect, lines: &[RenderedLine]) {
    for (row, line) in lines.iter().enumerate() {
        let y = area.y.saturating_add(row as u16);
        if y >= area.bottom() {
            break;
        }
        for link in &line.links {
            let x = area.x.saturating_add(link.start as u16);
            if x >= area.right() {
                continue;
            }
            let visible = link.width.min((area.right() - x) as usize);
            for offset in 0..visible {
                buffer[(x + offset as u16, y)]
                    .set_style(Style::default().add_modifier(Modifier::UNDERLINED));
            }
        }
    }
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
            SetAttribute(TerminalAttribute::Underlined),
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
    requests: Sender<Request>,
    responses: Receiver<Response>,
    link_requests: Sender<LinkRequest>,
    link_responses: Receiver<LinkResponse>,
    link_generation: u64,
    link_status: Option<String>,
    refresh_links_after_load: bool,
    quit: bool,
    scroll: usize,
    rendered: Vec<RenderedLine>,
    pr_links: BTreeMap<String, PullRequestLink>,
    help_open: bool,
    dirty: bool,
}

impl App {
    pub fn new(publish_options: SubmitOptions) -> Self {
        let worker_options = publish_options.clone();
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        thread::spawn(move || worker(worker_options, request_rx, response_tx));
        let link_worker_options = publish_options.clone();
        let (link_request_tx, link_request_rx) = mpsc::channel();
        let (link_response_tx, link_response_rx) = mpsc::channel();
        thread::spawn(move || link_worker(link_worker_options, link_request_rx, link_response_tx));
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
            requests: request_tx,
            responses: response_rx,
            link_requests: link_request_tx,
            link_responses: link_response_rx,
            link_generation: 0,
            link_status: None,
            refresh_links_after_load: false,
            quit: false,
            scroll: 0,
            rendered: Vec::new(),
            pr_links: BTreeMap::new(),
            help_open: false,
            dirty: true,
        };
        let _ = app.requests.send(Request::Load);
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

    fn request_pr_links(&mut self) {
        self.link_generation = self.link_generation.wrapping_add(1);
        self.pr_links.clear();
        self.link_status = None;
        if let Some(graph) = self.active_graph() {
            let _ = self.link_requests.send(link_request(
                self.link_generation,
                graph,
                &self.publish_options.remote,
            ));
        }
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
        if self.busy.is_some() {
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
        } else if self.requests.send(Request::Checkout(selected)).is_ok() {
            self.busy = Some(Busy::Mutation);
            self.status = Some("checking out…".into());
            self.status_error = false;
        }
    }

    fn apply(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(plan) = self.pending.take() else {
            return;
        };
        if self.requests.send(Request::Apply(plan)).is_ok() {
            self.busy = Some(Busy::Mutation);
            self.status = Some("applying…".into());
            self.status_error = false;
        }
    }

    fn refresh(&mut self) {
        if self.busy.is_some() {
            return;
        }
        self.clear_preview(false);
        self.status = Some("refreshing…".into());
        self.status_error = false;
        self.busy = Some(Busy::Load);
        self.refresh_links_after_load = self.requests.send(Request::Load).is_ok();
    }

    fn preview_publish(&mut self) {
        if self.busy.is_some() {
            return;
        }
        self.clear_preview(false);
        if self
            .requests
            .send(Request::PublishPreview(self.publish_options.clone()))
            .is_ok()
        {
            self.busy = Some(Busy::Load);
            self.status = Some("planning publish…".into());
            self.status_error = false;
        }
    }

    fn execute_publish(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(plan) = self.publish_plan.clone() else {
            self.status = Some("press p to preview publish changes first".into());
            self.status_error = true;
            return;
        };
        if self.requests.send(Request::PublishExecute(plan)).is_ok() {
            self.busy = Some(Busy::Mutation);
            self.status = Some("publishing…".into());
            self.status_error = false;
        }
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

    fn handle_search_key(&mut self, key: KeyEvent) {
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
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if let Some(open) = help_transition(self.help_open, &key) {
            self.help_open = open;
            return;
        }
        if self.search.editing {
            self.handle_search_key(key);
            return;
        }
        if let Some(substack) = move_pick(&key) {
            self.pick(substack);
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => self.move_cursor(-1),
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => self.move_cursor(1),
            (KeyCode::Enter, _) => self.enter(),
            (KeyCode::Char('a'), _) => self.apply(),
            (KeyCode::Char('p'), _) => self.preview_publish(),
            (KeyCode::Char('P'), _) => self.execute_publish(),
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
            (KeyCode::Char('r' | 'R'), _) => self.refresh(),
            (KeyCode::Char('q'), _) if self.busy == Some(Busy::Mutation) => {
                self.status = Some("a Git operation is still running".into());
                self.status_error = true;
            }
            (KeyCode::Char('q'), _) => self.quit = true,
            _ => {}
        }
    }

    fn receive(&mut self) {
        while let Ok(response) = self.responses.try_recv() {
            let mutation = self.busy == Some(Busy::Mutation);
            let published_links = response.published_links;
            self.busy = None;
            match response.graph {
                Ok(graph) => {
                    let old = self.selected.clone();
                    self.graph = Some(graph);
                    self.preview = response.preview;
                    self.publish_plan = response.publish_plan;
                    if let Some(links) = published_links {
                        self.link_generation = self.link_generation.wrapping_add(1);
                        self.pr_links = links;
                        self.link_status = None;
                    }
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
                    if std::mem::take(&mut self.refresh_links_after_load) {
                        self.request_pr_links();
                    }
                    self.update_rendered();
                }
                Err(error) => {
                    self.refresh_links_after_load = false;
                    self.link_generation = self.link_generation.wrapping_add(1);
                    if let Some(links) = published_links {
                        self.pr_links = links;
                    } else {
                        self.pr_links.clear();
                    }
                    self.link_status = None;
                    self.status = Some(response.operation_error.unwrap_or(error));
                    self.status_error = true;
                    self.update_rendered();
                }
            }
        }
        while let Ok(response) = self.link_responses.try_recv() {
            if !current_link_response(self.link_generation, &response) {
                continue;
            }
            match response.result {
                Ok(links) => {
                    self.pr_links = links;
                    if self
                        .link_status
                        .as_ref()
                        .is_some_and(|message| self.status.as_ref() == Some(message))
                    {
                        self.status = None;
                    }
                    self.link_status = None;
                    self.update_rendered();
                }
                Err(error) => {
                    self.pr_links.clear();
                    if self.status.is_none() {
                        self.status = Some(error.clone());
                        self.status_error = false;
                        self.link_status = Some(error);
                    }
                    self.update_rendered();
                }
            }
        }
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
                    Line::styled(
                        line.text.clone(),
                        graph_line_style(line, selected.as_ref(), &carried_commits),
                    )
                })
                .collect();
            frame.render_widget(Paragraph::new(lines).block(Block::default()), graph_area);
            underline_links(frame.buffer_mut(), graph_area, &visible);
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
                        style,
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
        let result = (|| -> Result<(), String> {
            while !self.quit {
                self.receive();
                if self.dirty {
                    self.draw(&mut terminal)
                        .map_err(|error| error.to_string())?;
                    self.dirty = false;
                }
                if event::poll(Duration::from_millis(50)).map_err(|error| error.to_string())? {
                    match event::read().map_err(|error| error.to_string())? {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            self.handle_key(key);
                            self.dirty = true;
                        }
                        Event::Resize(_, _) => self.dirty = true,
                        _ => {}
                    }
                }
            }
            Ok(())
        })();
        let _ = self.requests.send(Request::Stop);
        let _ = self.link_requests.send(LinkRequest::Stop);
        let _ = disable_raw_mode();
        let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
        let _ = terminal.show_cursor();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> (
        App,
        Receiver<Request>,
        Sender<Response>,
        Receiver<LinkRequest>,
    ) {
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        let (link_request_tx, link_request_rx) = mpsc::channel();
        let (_link_response_tx, link_response_rx) = mpsc::channel();
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
            requests: request_tx,
            responses: response_rx,
            link_requests: link_request_tx,
            link_responses: link_response_rx,
            link_generation: 0,
            link_status: None,
            refresh_links_after_load: false,
            quit: false,
            scroll: 0,
            rendered: Vec::new(),
            pr_links: BTreeMap::new(),
            help_open: false,
            dirty: false,
        };
        (app, request_rx, response_tx, link_request_rx)
    }

    fn graph_response() -> Response {
        Response {
            graph: Ok(Graph::default()),
            preview: None,
            publish_plan: None,
            published_links: None,
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
        assert!(footer.contains("p/P publish"));
        assert!(footer.contains("? help"));
        assert!(footer.contains("q quit"));
        assert!(!footer.contains("search"));

        let help = help_lines()
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        for binding in [
            "m move commit",
            "P execute publish",
            "/ search",
            "r/R refresh",
        ] {
            assert!(help.contains(binding), "missing {binding} from {help}");
        }
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
    fn hyperlink_rendering_underlines_cells_without_embedding_escape_sequences() {
        let area = Rect::new(0, 0, 40, 1);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(0, 0, "xxorigin/fs-head/topic/1", Style::default());
        let lines = vec![RenderedLine {
            text: "xxorigin/fs-head/topic/1".into(),
            commit: Some("commit".into()),
            preview: false,
            conflict: false,
            links: vec![crate::ui::render::RenderedLink {
                start: 2,
                width: 22,
                text: "origin/fs-head/topic/1".into(),
                url: "https://example.invalid/1".into(),
            }],
        }];

        underline_links(&mut buffer, area, &lines);

        assert_eq!(buffer[(2, 0)].symbol(), "o");
        assert!(
            buffer[(2, 0)]
                .style()
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(
            buffer[(23, 0)]
                .style()
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
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
    }

    #[test]
    fn stale_link_generations_are_rejected() {
        let stale = LinkResponse {
            generation: 4,
            result: Ok(BTreeMap::new()),
        };
        let current = LinkResponse {
            generation: 5,
            result: Ok(BTreeMap::new()),
        };

        assert!(!current_link_response(5, &stale));
        assert!(current_link_response(5, &current));
    }

    #[test]
    fn graph_load_does_not_fetch_pr_links_without_explicit_refresh() {
        let (mut app, _requests, responses, link_requests) = test_app();
        responses.send(graph_response()).unwrap();

        app.receive();

        assert!(matches!(
            link_requests.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn explicit_refresh_fetches_pr_links_once_after_loading_the_graph() {
        let (mut app, requests, responses, link_requests) = test_app();

        app.refresh();
        assert!(matches!(requests.recv().unwrap(), Request::Load));
        responses.send(graph_response()).unwrap();
        app.receive();

        assert!(matches!(
            link_requests.recv().unwrap(),
            LinkRequest::Load { generation: 1, .. }
        ));
        assert!(matches!(
            link_requests.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn normal_graph_updates_keep_existing_pr_links_without_refetching() {
        let (mut app, _requests, responses, link_requests) = test_app();
        app.pr_links.insert(
            "origin/fs-head/topic/1".into(),
            PullRequestLink {
                number: 1,
                head_ref_name: "fs-head/topic/1".into(),
                url: "https://example.invalid/1".into(),
            },
        );
        responses.send(graph_response()).unwrap();

        app.receive();

        assert_eq!(app.pr_links.len(), 1);
        assert!(matches!(
            link_requests.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn successful_publish_updates_pr_links_without_refetching() {
        let (mut app, _requests, responses, link_requests) = test_app();
        let link = PullRequestLink {
            number: 1,
            head_ref_name: "fs-head/topic/1".into(),
            url: "https://example.invalid/1".into(),
        };
        responses
            .send(Response {
                published_links: Some([("origin/fs-head/topic/1".into(), link.clone())].into()),
                ..graph_response()
            })
            .unwrap();

        app.receive();

        assert_eq!(app.pr_links.get("origin/fs-head/topic/1"), Some(&link));
        assert_eq!(app.link_generation, 1);
        assert!(matches!(
            link_requests.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn link_request_payload_contains_only_deduplicated_head_names() {
        let graph = Graph {
            commits: [
                (
                    "a".into(),
                    crate::ui::model::Commit {
                        remote_refs: vec![
                            "origin/fs-head/topic/1".into(),
                            "origin/fs-base/topic/2".into(),
                        ],
                        ..crate::ui::model::Commit::default()
                    },
                ),
                (
                    "b".into(),
                    crate::ui::model::Commit {
                        remote_refs: vec!["origin/fs-head/topic/1".into()],
                        ..crate::ui::model::Commit::default()
                    },
                ),
            ]
            .into(),
            ..Graph::default()
        };

        let LinkRequest::Load { generation, heads } = link_request(9, &graph, "origin") else {
            panic!("expected a load request");
        };
        assert_eq!(generation, 9);
        assert_eq!(heads, ["fs-head/topic/1"]);
    }
}
