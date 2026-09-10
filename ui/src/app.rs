use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::git::{Request, Response, worker};
use crate::model::{Graph, MovePlan};
use crate::render::{RenderedLine, commit_label, render_graph};

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

pub struct App {
    graph: Option<Graph>,
    preview: Option<Graph>,
    selected: Option<String>,
    carried: Option<String>,
    carry_substack: bool,
    pending: Option<MovePlan>,
    search: Search,
    status: Option<String>,
    busy: Option<Busy>,
    requests: Sender<Request>,
    responses: Receiver<Response>,
    quit: bool,
    scroll: usize,
    rendered: Vec<RenderedLine>,
    dirty: bool,
}

impl App {
    pub fn new(repo: PathBuf) -> Self {
        let (request_tx, request_rx) = mpsc::channel();
        let (response_tx, response_rx) = mpsc::channel();
        thread::spawn(move || worker(repo, request_rx, response_tx));
        let mut app = Self {
            graph: None,
            preview: None,
            selected: None,
            carried: None,
            carry_substack: false,
            pending: None,
            search: Search::default(),
            status: None,
            busy: Some(Busy::Load),
            requests: request_tx,
            responses: response_rx,
            quit: false,
            scroll: 0,
            rendered: Vec::new(),
            dirty: true,
        };
        let _ = app.requests.send(Request::Load);
        app
    }

    fn active_graph(&self) -> Option<&Graph> {
        self.preview.as_ref().or(self.graph.as_ref())
    }

    fn graph_ids(&self) -> Vec<String> {
        self.active_graph().map(|graph| graph.order.clone()).unwrap_or_default()
    }

    fn update_rendered(&mut self) {
        self.rendered = self.active_graph().map(render_graph).unwrap_or_default();
        self.dirty = true;
    }

    fn clear_preview(&mut self, keep_carried: bool) {
        let had_preview = self.preview.take().is_some();
        self.pending = None;
        if !keep_carried {
            self.carried = None;
        }
        if had_preview {
            self.update_rendered();
        }
    }

    fn move_cursor(&mut self, amount: isize) {
        self.clear_preview(true);
        let ids = self.graph_ids();
        if ids.is_empty() {
            return;
        }
        let current = self.selected.as_ref().and_then(|id| ids.iter().position(|item| item == id)).unwrap_or(0);
        let next = (current as isize + amount).clamp(0, ids.len() as isize - 1) as usize;
        self.selected = Some(ids[next].clone());
    }

    fn pick(&mut self, substack: bool) {
        self.clear_preview(false);
        self.carried = self.selected.clone();
        self.carry_substack = substack;
        self.status = None;
    }

    fn enter(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(selected) = self.selected.clone() else { return };
        if let Some(carried) = self.carried.clone() {
            let Some(graph) = self.graph.as_ref() else { return };
            match graph.plan_move(&carried, &selected, self.carry_substack).and_then(|plan| {
                let preview = graph.preview(&plan)?;
                Ok((plan, preview))
            }) {
                Ok((plan, preview)) => {
                    self.pending = Some(plan);
                    self.preview = Some(preview);
                    self.update_rendered();
                    self.status = None;
                }
                Err(error) => self.status = Some(error),
            }
        } else if self.requests.send(Request::Checkout(selected)).is_ok() {
            self.busy = Some(Busy::Mutation);
            self.status = Some("checking out…".into());
        }
    }

    fn apply(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let Some(plan) = self.pending.take() else { return };
        if self.requests.send(Request::Apply(plan)).is_ok() {
            self.busy = Some(Busy::Mutation);
            self.status = Some("applying…".into());
        }
    }

    fn refresh(&mut self) {
        if self.busy.is_some() {
            return;
        }
        self.clear_preview(false);
        self.status = Some("refreshing…".into());
        self.busy = Some(Busy::Load);
        let _ = self.requests.send(Request::Load);
    }

    fn update_search(&mut self) {
        let needle = self.search.query.to_lowercase();
        self.search.matches = self.graph.as_ref().map(|graph| {
            graph.order.iter().filter(|id| {
                graph.commits.get(*id).is_some_and(|commit| {
                    commit_label(commit, graph.branch.as_deref())
                        .to_lowercase()
                        .contains(&needle)
                })
            }).cloned().collect()
        }).unwrap_or_default();
    }

    fn next_match(&mut self, backwards: bool) {
        if self.search.matches.is_empty() {
            return;
        }
        let current = self.selected.as_ref().and_then(|id| self.search.matches.iter().position(|item| item == id));
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
        if self.search.editing {
            self.handle_search_key(key);
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => self.move_cursor(-1),
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => self.move_cursor(1),
            (KeyCode::Enter, _) => self.enter(),
            (KeyCode::Char(' '), modifiers) if modifiers.contains(KeyModifiers::SHIFT) => self.pick(true),
            (KeyCode::Char(' '), _) => self.pick(false),
            (KeyCode::Char('a'), _) => self.apply(),
            (KeyCode::Esc, _) => self.clear_preview(false),
            (KeyCode::Char('/'), _) => {
                self.clear_preview(true);
                self.search.editing = true;
                self.search.query.clear();
                self.search.matches.clear();
            }
            (KeyCode::Char('n'), modifiers) if modifiers.contains(KeyModifiers::SHIFT) => self.next_match(true),
            (KeyCode::Char('N'), _) => self.next_match(true),
            (KeyCode::Char('n'), _) => self.next_match(false),
            (KeyCode::Char('r' | 'R'), _) => self.refresh(),
            (KeyCode::Char('q'), _) if self.busy == Some(Busy::Mutation) => {
                self.status = Some("a Git operation is still running".into());
            }
            (KeyCode::Char('q'), _) => self.quit = true,
            _ => {}
        }
    }

    fn receive(&mut self) {
        while let Ok(response) = self.responses.try_recv() {
            let mutation = self.busy == Some(Busy::Mutation);
            self.busy = None;
            match response.graph {
                Ok(graph) => {
                    let old = self.selected.clone();
                    self.graph = Some(graph);
                    self.preview = None;
                    if mutation {
                        self.carried = None;
                        self.pending = None;
                    }
                    let ids = self.graph_ids();
                    self.selected = old.filter(|id| ids.contains(id)).or_else(|| ids.first().cloned());
                    self.status = response.operation_error;
                    self.update_rendered();
                }
                Err(error) => {
                    self.status = Some(response.operation_error.unwrap_or(error));
                    self.dirty = true;
                }
            }
        }
    }

    fn draw(&mut self, terminal: &mut Tui) -> io::Result<()> {
        let rendered = self.rendered.clone();
        let selected = self.selected.clone();
        let carried = self.carried.clone();
        terminal.draw(|frame| {
            let [graph_area, footer_area] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
            let height = graph_area.height as usize;
            let selected_row = rendered.iter().position(|line| line.commit.as_ref() == selected.as_ref()).unwrap_or(0);
            if selected_row < self.scroll {
                self.scroll = selected_row;
            } else if selected_row >= self.scroll.saturating_add(height) {
                self.scroll = selected_row.saturating_sub(height.saturating_sub(1));
            }
            let lines: Vec<Line> = rendered.iter().skip(self.scroll).take(height).map(|line| {
                let mut style = Style::default();
                if line.preview {
                    style = style.fg(Color::Cyan);
                }
                if line.conflict {
                    style = style.fg(Color::Red).add_modifier(Modifier::BOLD);
                }
                if line.commit.as_ref() == carried.as_ref() {
                    style = style.fg(Color::Yellow);
                }
                if line.commit.as_ref() == selected.as_ref() {
                    style = style.bg(Color::Rgb(64, 64, 64));
                }
                Line::styled(line.text.clone(), style)
            }).collect();
            frame.render_widget(Paragraph::new(lines).block(Block::default()), graph_area);

            let footer = if self.search.editing {
                Line::from(vec![Span::styled("/", Style::default().fg(Color::Cyan)), Span::raw(&self.search.query)])
            } else {
                let prefix = self.status.as_deref().map(|status| format!("{status}  ")).unwrap_or_default();
                Line::from(format!("{prefix}↑↓ select  Enter checkout/preview  Space commit  S-Space substack  a apply  Esc cancel  / search  n/N next/prev  R refresh  q quit"))
            };
            frame.render_widget(Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)), footer_area);
        })?;
        Ok(())
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
                    self.draw(&mut terminal).map_err(|error| error.to_string())?;
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
        let _ = disable_raw_mode();
        let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
        let _ = terminal.show_cursor();
        result
    }
}
