use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossterm::event::{self, Event};

use crate::core::submit::{SubmitOptions, SubmitPlan};
use crate::integrations::github::PullRequestLink;
use crate::ui::git::{
    apply_move, checkout, displayed_remote_heads, load_graph_for, load_pr_links, remote_pr_links,
};
use crate::ui::model::{Graph, MovePlan};

#[derive(Debug)]
pub(crate) enum Operation {
    Load,
    Refresh,
    Checkout {
        revision: String,
        branch: Option<String>,
    },
    Apply(MovePlan),
    PublishPreview(SubmitOptions),
    PublishExecute(SubmitPlan),
    Stop,
}

#[derive(Debug)]
pub(crate) struct OperationRequest {
    pub(crate) options: SubmitOptions,
    pub(crate) id: u64,
    pub(crate) operation: Operation,
}

#[derive(Debug)]
pub(crate) struct OperationResult {
    pub(crate) repo: PathBuf,
    pub(crate) id: u64,
    pub(crate) graph: Result<Graph, String>,
    pub(crate) preview: Option<Graph>,
    pub(crate) publish_plan: Option<SubmitPlan>,
    pub(crate) pr_links: Option<Result<BTreeMap<String, PullRequestLink>, String>>,
    pub(crate) operation_error: Option<String>,
}

#[derive(Debug)]
pub(crate) enum UiEvent {
    Terminal(Event),
    OperationCompleted(Box<OperationResult>),
    InputError(String),
}

pub(crate) struct EventLoop {
    pub(crate) operations: Sender<OperationRequest>,
    pub(crate) events: Receiver<UiEvent>,
    pub(crate) input_events: Sender<UiEvent>,
}

pub(crate) fn start() -> EventLoop {
    let (event_tx, event_rx) = mpsc::channel();
    let (operation_tx, operation_rx) = mpsc::channel();
    let operation_events = event_tx.clone();
    thread::spawn(move || operation_worker(operation_rx, operation_events));
    EventLoop {
        operations: operation_tx,
        events: event_rx,
        input_events: event_tx,
    }
}

pub(crate) struct InputWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for InputWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub(crate) fn start_input(events: Sender<UiEvent>) -> InputWorker {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let handle = thread::spawn(move || input_worker(events, worker_stop));
    InputWorker {
        stop,
        handle: Some(handle),
    }
}

fn input_worker(events: Sender<UiEvent>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match event::poll(Duration::from_millis(50)) {
            Ok(false) => continue,
            Ok(true) if stop.load(Ordering::Relaxed) => break,
            Ok(true) => match event::read() {
                Ok(event) => {
                    if events.send(UiEvent::Terminal(event)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = events.send(UiEvent::InputError(error.to_string()));
                    break;
                }
            },
            Err(error) => {
                let _ = events.send(UiEvent::InputError(error.to_string()));
                break;
            }
        }
    }
}

fn operation_worker(operations: Receiver<OperationRequest>, events: Sender<UiEvent>) {
    while let Ok(request) = operations.recv() {
        let options = request.options;
        let repo = options.repo.clone();
        let mut preview = None;
        let mut publish_plan = None;
        let mut pr_links = None;
        let mut loaded_graph = None;
        let operation_error = match request.operation {
            Operation::Load => None,
            Operation::Refresh => {
                let graph = load_graph_for(&repo, &options.remote, &options.base);
                if let Ok(graph) = &graph {
                    let heads = displayed_remote_heads(graph, &options.remote);
                    pr_links = Some(
                        load_pr_links(&options, &heads)
                            .map_err(|error| format!("PR links unavailable: {error}")),
                    );
                }
                loaded_graph = Some(graph);
                None
            }
            Operation::Checkout { revision, branch } => {
                checkout(&repo, &revision, branch.as_deref()).err()
            }
            Operation::Apply(plan) => apply_move(&repo, &plan).err(),
            Operation::PublishPreview(publish_options) => {
                match crate::core::submit::plan(publish_options) {
                    Ok(plan) => match load_graph_for(&repo, &options.remote, &options.base) {
                        Ok(graph) => match graph.publish_preview(&plan) {
                            Ok(publish_preview) => {
                                loaded_graph = Some(Ok(graph));
                                preview = Some(publish_preview);
                                publish_plan = Some(plan);
                                None
                            }
                            Err(error) => {
                                loaded_graph = Some(Ok(graph));
                                Some(error)
                            }
                        },
                        Err(error) => {
                            loaded_graph = Some(Err(error.clone()));
                            Some(error)
                        }
                    },
                    Err(error) => Some(error),
                }
            }
            Operation::PublishExecute(plan) => {
                match crate::core::submit::execute_checked_with_links(&plan) {
                    Ok(links) => {
                        pr_links = Some(Ok(remote_pr_links(&plan.options.remote, links)));
                        None
                    }
                    Err(error) => Some(error),
                }
            }
            Operation::Stop => break,
        };
        let graph =
            loaded_graph.unwrap_or_else(|| load_graph_for(&repo, &options.remote, &options.base));
        if events
            .send(UiEvent::OperationCompleted(Box::new(OperationResult {
                repo,
                id: request.id,
                graph,
                preview,
                publish_plan,
                pr_links,
                operation_error,
            })))
            .is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_completion_is_a_ui_event() {
        let (events, received) = mpsc::channel();
        let event = UiEvent::OperationCompleted(Box::new(OperationResult {
            repo: PathBuf::from("."),
            id: 0,
            graph: Ok(Graph::default()),
            preview: None,
            publish_plan: None,
            pr_links: None,
            operation_error: None,
        }));
        events.send(event).unwrap();
        assert!(matches!(
            received.recv().unwrap(),
            UiEvent::OperationCompleted(_)
        ));
    }
}
