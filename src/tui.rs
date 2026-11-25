//! Progressive session picker with an embedded SSH/tmux client.
//! The selector remains available while the client runs in its own PTY.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;

use crate::app::{AttachOutcome, Choice, Inventory, Status};
use crate::cancel::Cancel;
use crate::display::display;
use crate::embedded::EmbeddedTerminal;
use crate::exit_codes;
use crate::session_form::SessionForm;
use crate::state::{State, StateChange};
use crate::theme::Tint;
use crate::tmux::{AgentState, AgentStatusRow, NewSession, Session};
use crate::workers::Worker;

mod attention;
mod confirmation;
mod drawing;
mod input;
mod layout;
mod reporting_setup;
mod runtime;

pub use runtime::run_picker;

#[cfg(test)]
mod tests;

use attention::View;
use confirmation::EndConfirmation;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// A selection must rest this long before its preview is fetched.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(250);
/// A visible preview older than this is refetched while it stays selected.
const PREVIEW_REFRESH: Duration = Duration::from_secs(1);
const STATUS_REFRESH: Duration = Duration::from_secs(5);
/// How long the preview shows static after the selection changes.
const SELECTION_STATIC: Duration = Duration::from_millis(150);

type Discover = Arc<dyn Fn(&Cancel) -> Result<(Vec<Worker>, String), String> + Send + Sync>;
type Scan = Arc<dyn Fn(&Cancel, Vec<Worker>) -> mpsc::Receiver<Inventory> + Send + Sync>;
type Preview = Arc<
    dyn Fn(&Cancel, &Worker, &Session) -> Result<crate::tmux::Preview, (Status, String)>
        + Send
        + Sync,
>;
type AgentStatuses =
    Arc<dyn Fn(&Cancel, &Worker) -> Result<Vec<AgentStatusRow>, String> + Send + Sync>;
type SaveMapping = Box<dyn FnMut(&str, &str) -> Result<(), String>>;
type UpdateState = Box<dyn FnMut(StateChange<'_>) -> Result<State, String>>;

type ResolveUser = Arc<dyn Fn(&Cancel, &str) -> Result<String, String> + Send + Sync>;
type ReportingSetup = Arc<
    dyn Fn(&Cancel, &Worker, Option<&str>) -> Result<crate::reporting::Response, String>
        + Send
        + Sync,
>;

/// Discovery, access checks and persistence, injected for model tests.
struct Services {
    discover: Discover,
    scan: Scan,
    preview: Preview,
    agent_status: AgentStatuses,
    reporting_setup: ReportingSetup,
    resolve_user: ResolveUser,
    sharing_active: Box<dyn Fn(&str) -> bool>,
    save_mapping: SaveMapping,
    add_mapping: SaveMapping,
    add_computer_unavailable: Option<&'static str>,
    update_state: UpdateState,
    /// Unix seconds, for activity ages.
    now: Box<dyn Fn() -> i64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowKind {
    PinnedHeader,
    Separator,
    ComputersHeader,
    OtherHeader,
    Pinned,
    /// A computer's heading above its sessions.
    Group,
    Session,
    Computer,
    AttentionSession,
    ErrorsHeader,
    AttentionEmpty,
    CoverageHeader,
    NeedsInputHeader,
}

#[derive(Clone, Copy)]
struct Row {
    kind: RowKind,
    entry: usize,
    session: usize,
    shortcut: Option<u8>,
}

impl Row {
    fn new(kind: RowKind, entry: usize, session: usize) -> Self {
        Self {
            kind,
            entry,
            session,
            shortcut: None,
        }
    }
}

/// The selection by identity, so it survives row rebuilds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Selection {
    #[default]
    None,
    Computer(String),
    Attention(attention::Identity),
    Session {
        worker: String,
        session: String,
        pinned: bool,
    },
}

struct Entry {
    worker: Worker,
    inventory: Option<Inventory>,
}

impl Entry {
    fn update_inventory(&mut self, mut inventory: Inventory, reorder: bool) {
        if reorder || self.inventory.is_none() {
            // Only waiting calls for attention ahead of activity, not failed agents.
            inventory.sessions.sort_by_key(|session| {
                (
                    !reorder
                        || !session
                            .agent
                            .is_some_and(|agent| agent.state == AgentState::Waiting),
                    std::cmp::Reverse(session.activity),
                )
            });
        } else {
            let positions: HashMap<&str, usize> = self
                .sessions()
                .iter()
                .enumerate()
                .map(|(index, session)| (session.id.as_str(), index))
                .collect();
            // Existing sessions keep their places; new sessions append newest first.
            inventory.sessions.sort_by_key(|session| {
                (
                    positions
                        .get(session.id.as_str())
                        .copied()
                        .unwrap_or(usize::MAX),
                    std::cmp::Reverse(session.activity),
                )
            });
        }
        self.inventory = Some(inventory);
    }

    fn status(&self) -> Status {
        self.inventory
            .as_ref()
            .map_or(Status::Scanning, |inventory| inventory.status)
    }

    fn sessions(&self) -> &[Session] {
        match &self.inventory {
            Some(inventory) if inventory.status == Status::Ready => &inventory.sessions,
            _ => &[],
        }
    }

    /// A remote failure the user can fix from Delux: set the user or trust the key.
    fn needs_setup(&self) -> bool {
        self.worker.destination.is_some()
            && matches!(self.status(), Status::AuthRequired | Status::HostTrust)
    }

    /// Sessions first, then fixable computers, probing, empty, failed, and offline.
    fn rank(&self) -> u8 {
        match self.status() {
            _ if !self.sessions().is_empty() => 0,
            _ if self.worker.online == Some(false) => 5,
            _ if self.needs_setup() => 1,
            Status::Scanning => 2,
            Status::Ready => 3,
            _ => 4,
        }
    }
}

struct PreviewEntry {
    fetched: Instant,
    /// The captured session; a replacement may reuse its tmux ID.
    session: Session,
    result: Result<String, String>,
}

/// Preview cache key: worker key and session ID.
type PreviewKey = (String, String);

struct ActiveTerminal {
    worker: Worker,
    session: Option<Session>,
    started: Instant,
    terminal: EmbeddedTerminal,
    // Fields drop in declaration order: release the master only after the client.
    _attachment: crate::ssh_mux::AttachmentGuard,
}

impl ActiveTerminal {
    fn matches(&self, worker: &Worker, session: Option<&Session>) -> bool {
        self.worker.key == worker.key
            && self.worker.destination == worker.destination
            && match (&self.session, session) {
                (Some(active), Some(selected)) => same_session(active, selected),
                (None, None) => true,
                _ => false,
            }
    }
}

/// Whether two listings name the same session lifetime, not merely the same ID.
fn same_session(a: &Session, b: &Session) -> bool {
    a.id == b.id
        && a.created == b.created
        && a.server_pid == b.server_pid
        && a.server_start == b.server_start
        && a.socket_path == b.socket_path
}

struct UserInput {
    key: String,
    name: String,
    destination: String,
    text: String,
    connect: bool,
    ssh_user: Option<String>,
    lookup: Option<Cancel>,
}

impl UserInput {
    fn stop_lookup(&mut self) {
        if let Some(cancel) = self.lookup.take() {
            cancel.cancel();
        }
    }
}

impl Drop for UserInput {
    fn drop(&mut self) {
        self.stop_lookup();
    }
}

enum Message {
    DirectoryCompleted {
        epoch: u64,
        worker: Worker,
        path: String,
        result: Result<crate::directory_completion::DirectoryCompletion, String>,
    },
    ReportingSetup {
        epoch: u64,
        worker: Worker,
        result: Result<crate::reporting::Response, String>,
    },
    SshUser {
        worker: Worker,
        cancel: Cancel,
        result: Result<String, String>,
    },
    Discovered {
        generation: u64,
        result: Result<(Vec<Worker>, String), String>,
    },
    Inventory {
        generation: u64,
        inventory: Option<Inventory>,
    },
    AgentStatuses {
        generation: u64,
        results: Vec<(Worker, Result<Vec<AgentStatusRow>, String>)>,
    },
    Preview {
        generation: u64,
        destination: Option<String>,
        key: PreviewKey,
        session: Session,
        result: Result<crate::tmux::Preview, (Status, String)>,
    },
    Created {
        worker: Worker,
        result: Result<(Session, Vec<Session>), (Status, String)>,
    },
    Ended {
        worker: Worker,
        session: Session,
        result: Result<(), (Status, String)>,
    },
    Attached(Result<AttachOutcome, String>),
    Checked(Inventory),
    Spinner,
    Tick(Instant),
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
}

enum Command {
    CompleteDirectory {
        epoch: u64,
        worker: Worker,
        path: String,
        cancel: Cancel,
    },
    ReportingSetup {
        epoch: u64,
        cancel: Cancel,
        workers: Vec<Worker>,
        token: Option<String>,
    },
    ResolveUser {
        worker: Worker,
        cancel: Cancel,
    },
    Discover {
        generation: u64,
        cancel: Cancel,
    },
    Scan {
        generation: u64,
        cancel: Cancel,
        workers: Vec<Worker>,
    },
    AgentStatuses {
        generation: u64,
        cancel: Cancel,
        workers: Vec<Worker>,
    },
    Preview {
        worker: Worker,
        session: Session,
        cancel: Cancel,
    },
    Create {
        worker: Worker,
        request: NewSession,
        cancel: Cancel,
    },
    End {
        worker: Worker,
        session: Session,
        cancel: Cancel,
    },
    Launch(Choice),
}

struct Picker {
    cancel: Cancel,
    services: Services,
    generation: u64,
    scan_cancel: Option<Cancel>,
    scanning: bool,
    reorder_on_scan: bool,
    status_inflight: Option<u64>,
    status_refreshed: Option<Instant>,
    pending: HashSet<String>,
    frame: usize,
    entries: Vec<Entry>,
    warning: String,
    notice: String,
    show_scroll_hint: bool,
    filter: String,
    view: View,
    other_selection: Selection,
    other_filter: String,
    attention_order: Vec<attention::Identity>,
    errors_expanded: bool,
    attention_waiting: usize,
    checks_expanded: bool,
    status_errors: HashMap<String, String>,
    status_pending: HashSet<String>,
    view_area: Rect,
    setup_area: Rect,
    setup_epoch: u64,
    reporting_setup: Option<reporting_setup::Setup>,
    filtering: bool,
    input: Option<UserInput>,
    creation: Option<SessionForm>,
    creation_pending: Option<Cancel>,
    directory_completion_epoch: u64,
    directory_completion_pending: Option<Cancel>,
    ending: Option<EndConfirmation>,
    ending_pending: Option<Cancel>,
    add_input: Option<String>,
    select_added: Option<String>,
    rows: Vec<Row>,
    cursor: Option<usize>,
    list_state: ListState,
    list_height: u16,
    list_area: Rect,
    selection: Selection,
    state: State,
    state_error: String,
    state_writable: bool,
    previews: HashMap<PreviewKey, PreviewEntry>,
    preview_inflight: Option<(PreviewKey, Cancel)>,
    preview_target: Option<(PreviewKey, Instant)>,
    preview_visible: bool,
    active: Option<ActiveTerminal>,
    terminal_focused: bool,
    terminal_visible: bool,
    terminal_area: Rect,
    terminal_error: Option<String>,
    clock: Option<Instant>,
    tint: Tint,
    /// The selection the preview last tuned to; a change shows static.
    screen_selection: Selection,
    static_until: Option<Instant>,
    static_seed: u64,
    quit: bool,
    code: i32,
    error: Option<String>,
}

impl Picker {
    fn new(cancel: Cancel, services: Services, state: Result<State, String>) -> Self {
        let state_writable = state.is_ok();
        let (state, state_error) = match state {
            Ok(state) => (state, String::new()),
            Err(error) => (State::default(), error),
        };
        Self {
            cancel: cancel.child(),
            services,
            generation: 0,
            scan_cancel: None,
            scanning: false,
            reorder_on_scan: false,
            pending: HashSet::new(),
            status_inflight: None,
            status_refreshed: None,
            frame: 0,
            entries: Vec::new(),
            warning: String::new(),
            notice: String::new(),
            show_scroll_hint: false,
            filter: String::new(),
            view: View::Browse,
            other_selection: Selection::None,
            other_filter: String::new(),
            attention_order: Vec::new(),
            errors_expanded: false,
            attention_waiting: 0,
            checks_expanded: false,
            status_errors: HashMap::new(),
            status_pending: HashSet::new(),
            view_area: Rect::default(),
            setup_area: Rect::default(),
            setup_epoch: 0,
            reporting_setup: None,
            filtering: false,
            input: None,
            creation: None,
            creation_pending: None,
            directory_completion_epoch: 0,
            directory_completion_pending: None,
            ending: None,
            ending_pending: None,
            add_input: None,
            select_added: None,
            rows: Vec::new(),
            cursor: None,
            list_state: ListState::default(),
            list_height: 0,
            list_area: Rect::default(),
            selection: Selection::None,
            state,
            state_error,
            state_writable,
            previews: HashMap::new(),
            preview_inflight: None,
            preview_target: None,
            preview_visible: false,
            active: None,
            terminal_focused: false,
            terminal_visible: false,
            terminal_area: Rect::default(),
            terminal_error: None,
            clock: None,
            tint: Tint::default(),
            screen_selection: Selection::None,
            static_until: None,
            static_seed: 0,
            quit: false,
            code: 0,
            error: None,
        }
    }

    fn rescan(&mut self, reorder: bool) -> Command {
        if let Some(cancel) = self.scan_cancel.take() {
            cancel.cancel();
        }
        if let Some((_, cancel)) = self.preview_inflight.take() {
            cancel.cancel();
        }
        self.preview_target = None;
        let cancel = self.cancel.child();
        self.scan_cancel = Some(cancel.clone());
        self.generation += 1;
        self.status_pending.clear();
        self.status_inflight = None;
        self.scanning = true;
        self.reorder_on_scan = reorder;
        Command::Discover {
            generation: self.generation,
            cancel,
        }
    }

    fn quit(&mut self) {
        self.cancel.cancel();
        self.quit = true;
    }

    fn update(&mut self, message: Message) -> Option<Command> {
        match message {
            Message::DirectoryCompleted {
                epoch,
                worker,
                path,
                result,
            } => {
                if epoch != self.directory_completion_epoch {
                    return None;
                }
                let pending = self.directory_completion_pending.take()?;
                if pending.is_cancelled() {
                    return None;
                }
                let form = self.creation.as_mut()?;
                if form.worker.key != worker.key || form.worker.destination != worker.destination {
                    return None;
                }
                let unchanged = self.entries.iter().any(|entry| {
                    entry.worker.key == worker.key
                        && entry.worker.destination == worker.destination
                        && entry.status() == Status::Ready
                });
                form.complete_directory(
                    &path,
                    if unchanged {
                        result
                    } else {
                        Err("Computer changed or is no longer ready. Check the connection and try again.".into())
                    },
                );
            }
            Message::ReportingSetup {
                epoch,
                worker,
                result,
            } => {
                self.reporting_result(epoch, worker, result);
            }
            Message::SshUser {
                worker,
                cancel,
                result,
            } => {
                if cancel.is_cancelled() {
                    return None;
                }
                let input = self.input.as_mut()?;
                if input.lookup.is_none()
                    || input.key != worker.key
                    || Some(&input.destination) != worker.destination.as_ref()
                {
                    return None;
                }
                input.lookup = None;
                match result {
                    Ok(user) => {
                        input.text = user.clone();
                        input.ssh_user = Some(user);
                        self.notice.clear();
                    }
                    Err(error) => {
                        self.notice = format!(
                            "{} Enter the remote username manually.",
                            display(&error, 300),
                        );
                    }
                }
            }
            Message::Discovered { generation, result } => {
                if generation != self.generation {
                    return None;
                }
                let (workers, warning) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        self.code = 1;
                        self.error = Some(error);
                        self.quit();
                        return None;
                    }
                };
                self.warning = warning;
                self.pending = workers.iter().map(|worker| worker.key.clone()).collect();
                let mut discovered: HashMap<&str, &Worker> = workers
                    .iter()
                    .map(|worker| (worker.key.as_str(), worker))
                    .collect();
                self.entries.retain_mut(|entry| {
                    let Some(worker) = discovered.remove(entry.worker.key.as_str()) else {
                        return false;
                    };
                    if entry.worker.destination != worker.destination {
                        self.status_errors.remove(&entry.worker.key);
                        entry.inventory = None;
                    }
                    entry.worker = worker.clone();
                    true
                });
                self.entries.extend(
                    workers
                        .iter()
                        .filter(|worker| discovered.contains_key(worker.key.as_str()))
                        .map(|worker| Entry {
                            worker: worker.clone(),
                            inventory: None,
                        }),
                );
                if let Some(destination) = self.select_added.take() {
                    if self.view == View::Attention {
                        self.switch_view();
                    }
                    if let Some(entry) = self.entries.iter().find(|entry| {
                        entry.worker.destination.as_deref() == Some(destination.as_str())
                    }) {
                        self.selection = Selection::Computer(entry.worker.key.clone());
                        self.filter.clear();
                        self.filtering = false;
                    } else {
                        self.notice = format!(
                            "Saved {} in cli.json, but discovery excluded it; check configured ignore entries.",
                            display(&destination, 200),
                        );
                    }
                }
                self.rebuild();
                if workers.is_empty() {
                    self.scanning = false;
                    self.reorder_on_scan = false;
                } else if let Some(cancel) = &self.scan_cancel {
                    return Some(Command::Scan {
                        generation,
                        cancel: cancel.clone(),
                        workers,
                    });
                }
            }
            Message::Inventory {
                generation,
                inventory,
            } => {
                if generation != self.generation {
                    return None;
                }
                let Some(inventory) = inventory else {
                    for entry in &mut self.entries {
                        if self.pending.contains(&entry.worker.key) {
                            entry.inventory = Some(Inventory {
                                worker: entry.worker.clone(),
                                sessions: Vec::new(),
                                status: Status::ProbeFailed,
                                detail: "No inventory received from the completed check.".into(),
                            });
                        }
                    }
                    self.scanning = false;
                    self.reorder_on_scan = false;
                    self.pending.clear();
                    self.rebuild();
                    return None;
                };
                if self.pending.remove(&inventory.worker.key)
                    && let Some(entry) = self.entries.iter_mut().find(|entry| {
                        entry.worker.key == inventory.worker.key
                            && entry.worker.destination == inventory.worker.destination
                    })
                {
                    self.status_errors.remove(&entry.worker.key);
                    entry.update_inventory(inventory, self.reorder_on_scan);
                }
                if self.reorder_on_scan {
                    self.entries.sort_by_key(Entry::rank);
                }
                self.rebuild();
            }
            Message::AgentStatuses {
                generation,
                results,
            } => {
                if self.status_inflight == Some(generation) {
                    self.status_inflight = None;
                }
                if generation != self.generation || self.quit {
                    return None;
                }
                for (worker, result) in results {
                    self.status_pending.remove(&worker.key);
                    let Some(inventory) = self
                        .entries
                        .iter_mut()
                        .find(|entry| {
                            entry.worker.key == worker.key
                                && entry.worker.destination == worker.destination
                        })
                        .and_then(|entry| entry.inventory.as_mut())
                        .filter(|inventory| inventory.status == Status::Ready)
                    else {
                        continue;
                    };
                    let rows = match result {
                        Ok(rows) => {
                            self.status_errors.remove(&worker.key);
                            rows
                        }
                        Err(error) => {
                            self.status_errors.insert(worker.key, error);
                            for session in &mut inventory.sessions {
                                session.agent = None;
                            }
                            continue;
                        }
                    };
                    for session in &mut inventory.sessions {
                        let row = rows.iter().find(|row| {
                            session.id == row.session_id
                                && session.server_pid == row.server_pid
                                && session.server_start == row.server_start
                        });
                        session.agent = row.and_then(|row| row.agent);
                        if let Some(row) = row {
                            session.output = session.output.max(row.output);
                        }
                    }
                }
                self.rebuild();
            }
            Message::Preview {
                generation,
                destination,
                key,
                session,
                result,
            } => {
                if generation != self.generation || self.shows_terminal() {
                    return None;
                }
                if self
                    .preview_inflight
                    .as_ref()
                    .is_some_and(|(inflight, _)| *inflight == key)
                {
                    self.preview_inflight = None;
                }
                let inventory = self
                    .entries
                    .iter_mut()
                    .find(|entry| {
                        entry.worker.key == key.0 && entry.worker.destination == destination
                    })
                    .and_then(|entry| entry.inventory.as_mut())?;
                let current = inventory
                    .sessions
                    .iter_mut()
                    .find(|current| same_session(current, &session))?;
                if destination.is_some()
                    && let Err((status @ (Status::AuthRequired | Status::HostTrust), detail)) =
                        &result
                {
                    inventory.status = *status;
                    inventory.detail = detail.clone();
                    inventory.sessions.clear();
                    self.previews.retain(|(worker, _), _| *worker != key.0);
                    if matches!(&self.selection, Selection::Session { worker, .. } if *worker == key.0)
                    {
                        self.selection = Selection::Computer(key.0);
                        self.filter.clear();
                        self.filtering = false;
                    }
                    self.rebuild();
                    return None;
                }
                if let Ok(preview) = &result {
                    current.activity = preview.activity;
                    current.output = preview.output;
                }
                let result = result
                    .map(|preview| preview.text)
                    .map_err(|(_, detail)| detail);
                let fetched = self.clock.unwrap_or_else(Instant::now);
                self.previews.insert(
                    key,
                    PreviewEntry {
                        fetched,
                        session,
                        result,
                    },
                );
            }
            Message::Created { worker, result } => {
                let pending = self.creation_pending.take()?;
                if pending.is_cancelled() {
                    self.notice = "Creation stopped; it may already exist. Rescanning.".into();
                    return Some(self.rescan(false));
                }
                let Some(entry) = self.entries.iter_mut().find(|entry| {
                    entry.worker.key == worker.key && entry.worker.destination == worker.destination
                }) else {
                    self.creation = None;
                    self.notice = "Computer changed; rescan before trying creation again.".into();
                    return Some(self.rescan(false));
                };
                match result {
                    Ok((session, sessions)) => {
                        // A client's exit can start a scan while creation is
                        // running. Ignore its older inventory for this computer,
                        // but let the remaining computers finish refreshing.
                        self.pending.remove(&worker.key);
                        entry.update_inventory(
                            Inventory {
                                worker: worker.clone(),
                                sessions,
                                status: Status::Ready,
                                detail: String::new(),
                            },
                            false,
                        );
                        self.creation = None;
                        self.show_session(&worker.key, &session.id);
                        self.notice.clear();
                        return Some(Command::Launch(Choice {
                            worker,
                            session: Some(session),
                        }));
                    }
                    Err((status, detail)) => {
                        let needs_sign_in = worker.destination.is_some()
                            && matches!(status, Status::AuthRequired | Status::HostTrust);
                        if needs_sign_in {
                            entry.inventory = Some(Inventory {
                                worker,
                                sessions: Vec::new(),
                                status,
                                detail: detail.clone(),
                            });
                        }
                        if let Some(form) = &mut self.creation {
                            form.busy = false;
                            form.error = display(&detail, 600);
                            if needs_sign_in {
                                form.error
                                    .push_str(" Esc to return, then Enter to sign in.");
                            }
                        }
                        self.rebuild();
                        // A failed response cannot prove the remote mutation did not commit.
                        return Some(self.rescan(false));
                    }
                }
            }
            Message::Ended {
                worker,
                session,
                result,
            } => {
                self.ending_pending.take()?;
                self.ending = None;
                match result {
                    Ok(()) => {
                        if self
                            .active
                            .as_ref()
                            .is_some_and(|active| active.matches(&worker, Some(&session)))
                        {
                            // Drop only this client; tmux may otherwise switch it
                            // to a surviving session when detach-on-destroy is off.
                            self.active = None;
                            self.terminal_focused = false;
                            self.terminal_visible = false;
                            self.terminal_error = None;
                        }
                        if let Some(inventory) = self
                            .entries
                            .iter_mut()
                            .find(|entry| {
                                entry.worker.key == worker.key
                                    && entry.worker.destination == worker.destination
                            })
                            .and_then(|entry| entry.inventory.as_mut())
                        {
                            inventory
                                .sessions
                                .retain(|current| !same_session(current, &session));
                        }
                        self.previews.remove(&(worker.key, session.id));
                        self.notice = format!(
                            "Ended {} on {}. Rescanning.",
                            display(&session.name, 120),
                            display(&worker.name, 120),
                        );
                        self.rebuild();
                    }
                    Err((_, detail)) => {
                        self.notice = format!(
                            "End session: {} Rescanning; no retry was sent.",
                            display(&detail, 600)
                        );
                    }
                }
                // A terminal exit may already have started a pre-mutation scan.
                // Its results must not put the ended session back in the list.
                return Some(self.rescan(false));
            }
            Message::Attached(result) => {
                self.active = None;
                self.terminal_focused = false;
                self.terminal_visible = false;
                self.terminal_error = None;
                self.previews.clear();
                self.preview_target = None;
                self.notice = match result {
                    Err(error) => format!("Attach failed: {}", display(&error, 400)),
                    Ok(AttachOutcome::Switched) => {
                        self.quit();
                        return None;
                    }
                    Ok(AttachOutcome::Exited(0)) => String::new(),
                    Ok(AttachOutcome::Exited(exit_codes::SESSION_CHANGED)) => {
                        "Session changed; rescanning.".to_owned()
                    }
                    Ok(AttachOutcome::Exited(code)) => format!(
                        "Attachment ended with status {code}; no replacement session was created."
                    ),
                };
                return Some(self.rescan(false));
            }
            Message::Checked(inventory) => {
                if !self.entries.iter().any(|entry| {
                    entry.worker.key == inventory.worker.key
                        && entry.worker.destination == inventory.worker.destination
                }) {
                    self.notice = "Computer configuration changed; rescanning.".into();
                    return Some(self.rescan(false));
                }
                let name = display(&inventory.worker.name, 80);
                let session = (inventory.status == Status::Ready)
                    .then(|| most_recent(&inventory.sessions).cloned())
                    .flatten();
                self.notice = match (&session, inventory.status) {
                    (Some(_), _) => format!("{name} is ready."),
                    (None, Status::Ready) => format!("{name} is ready but has no tmux sessions."),
                    (None, status) => format!("{name}: {status}"),
                };
                let worker = inventory.worker.key.clone();
                if let Some(entry) = self.entries.iter_mut().find(|entry| {
                    entry.worker.key == worker
                        && entry.worker.destination == inventory.worker.destination
                }) {
                    self.status_errors.remove(&worker);
                    entry.update_inventory(inventory, false);
                }
                // Do not steal selection from someone browsing during sign-in.
                if let Some(session) = session
                    && self.selection == Selection::Computer(worker.clone())
                {
                    self.show_session(&worker, &session.id);
                } else {
                    self.rebuild();
                }
            }
            Message::Spinner => self.frame = (self.frame + 1) % SPINNER.len(),
            Message::Tick(now) => return self.tick(now),
            Message::Key(key) => return self.key(key),
            Message::Mouse(event) => return self.mouse(event),
            Message::Paste(text) => return self.paste(&text),
        }
        None
    }

    /// Selects a newly available session in its computer's expanded group.
    fn show_session(&mut self, worker: &str, session: &str) {
        if self.view == View::Attention {
            self.switch_view();
        }
        self.selection = Selection::Session {
            worker: worker.to_owned(),
            session: session.to_owned(),
            pinned: false,
        };
        self.filter.clear();
        self.filtering = false;
        self.rebuild();
    }

    /// Advances the preview, and on ticks where the preview needs nothing,
    /// starts a due status refresh. A tick carries one command, and the
    /// selection's preview must never wait behind a background refresh.
    fn tick(&mut self, now: Instant) -> Option<Command> {
        self.clock = Some(now);
        self.static_seed = self.static_seed.wrapping_add(1);
        let refreshed = *self.status_refreshed.get_or_insert(now);
        self.preview_tick(now)
            .or_else(|| self.status_tick(now, refreshed))
    }

    fn status_tick(&mut self, now: Instant, refreshed: Instant) -> Option<Command> {
        if self.quit
            || self.scanning
            || self.status_inflight.is_some()
            || (now.saturating_duration_since(refreshed) < STATUS_REFRESH
                && self.status_pending.is_empty())
        {
            return None;
        }
        self.status_refreshed = Some(now);
        let workers: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| {
                entry.status() == Status::Ready
                    && (self.view == View::Attention || !entry.sessions().is_empty())
            })
            .map(|entry| entry.worker.clone())
            .collect();
        if workers.is_empty() {
            return None;
        }
        self.status_inflight = Some(self.generation);
        self.status_pending = workers.iter().map(|worker| worker.key.clone()).collect();
        Some(Command::AgentStatuses {
            generation: self.generation,
            cancel: self.cancel.child(),
            workers,
        })
    }

    /// Fetches the selected session's preview once the selection has rested,
    /// and refreshes it while it stays selected. Only one fetch runs at a time.
    fn preview_tick(&mut self, now: Instant) -> Option<Command> {
        let previewing = self.creation.is_none()
            && self.ending.is_none()
            && self.preview_visible
            && !self.shows_terminal();
        if previewing && self.selection != self.screen_selection {
            self.screen_selection = self.selection.clone();
            self.static_until = Some(now + SELECTION_STATIC);
        }
        let selected = if previewing {
            self.selected_session()
        } else {
            None
        };
        let Some((worker, session)) = selected else {
            self.preview_target = None;
            if let Some((_, cancel)) = self.preview_inflight.take() {
                cancel.cancel();
            }
            return None;
        };
        let key = (worker.key.clone(), session.id.clone());
        match &self.preview_target {
            Some((target, _)) if *target == key => {}
            _ => {
                self.preview_target = Some((key.clone(), now));
                if let Some((inflight, cancel)) = &self.preview_inflight
                    && *inflight != key
                {
                    cancel.cancel();
                    self.preview_inflight = None;
                }
            }
        }
        let rested = self
            .preview_target
            .as_ref()
            .is_some_and(|(_, since)| now.saturating_duration_since(*since) >= PREVIEW_DEBOUNCE);
        let fresh = self.cached_preview(&key, &session).is_some_and(|entry| {
            let shared = worker
                .destination
                .as_deref()
                .is_some_and(|destination| (self.services.sharing_active)(destination));
            now.saturating_duration_since(entry.fetched)
                < preview_refresh(worker.destination.is_none(), shared, entry.result.is_ok())
        });
        if !rested || fresh || self.preview_inflight.is_some() {
            return None;
        }
        let cancel = self.cancel.child();
        self.preview_inflight = Some((key, cancel.clone()));
        Some(Command::Preview {
            worker,
            session,
            cancel,
        })
    }

    fn toggle_pin(&mut self) {
        let Some(row) = self.current_row() else {
            return;
        };
        let Some(session) = self.session(row).cloned() else {
            return;
        };
        let worker = self.entries[row.entry].worker.clone();
        self.persist_state(StateChange::TogglePin {
            worker: &worker.key,
            session: &session.name,
        });
        let pinned = self.state.is_pinned(&worker.key, &session.name);
        self.notice = format!(
            "{} {} on {}.",
            if pinned { "★ Pinned" } else { "Unpinned" },
            display(&session.name, 80),
            display(&worker.name, 80)
        );
        self.rebuild();
    }

    fn persist_state(&mut self, change: StateChange<'_>) {
        if !self.state_writable {
            self.state.apply(change);
            return;
        }
        match (self.services.update_state)(change) {
            Ok(state) => {
                self.state = state;
                self.state_error.clear();
            }
            Err(error) => {
                self.state.apply(change);
                self.state_error = format!("Could not save pins: {error}");
            }
        }
    }

    fn page(&self) -> isize {
        self.list_height.saturating_sub(2).max(1) as isize
    }

    fn current_row(&self) -> Option<Row> {
        self.rows.get(self.cursor?).copied()
    }

    fn selectable(row: Row) -> bool {
        matches!(
            row.kind,
            RowKind::Session | RowKind::Pinned | RowKind::AttentionSession | RowKind::Computer
        )
    }

    fn session(&self, row: Row) -> Option<&Session> {
        if !matches!(
            row.kind,
            RowKind::Session | RowKind::Pinned | RowKind::AttentionSession
        ) {
            return None;
        }
        self.entries[row.entry].sessions().get(row.session)
    }

    fn selected_session(&self) -> Option<(Worker, Session)> {
        let row = self.current_row()?;
        let session = self.session(row)?.clone();
        Some((self.entries[row.entry].worker.clone(), session))
    }

    fn shows_terminal(&self) -> bool {
        self.active.as_ref().is_some_and(|active| {
            self.terminal_focused
                || self.current_row().is_none()
                || self.current_row().is_some_and(|row| {
                    active.matches(&self.entries[row.entry].worker, self.session(row))
                })
        })
    }

    fn move_cursor(&mut self, delta: isize) {
        let choices: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| Self::selectable(*row).then_some(i))
            .collect();
        if choices.is_empty() {
            return;
        }
        let position = choices.iter().position(|i| Some(*i) == self.cursor);
        let position = match position {
            Some(position) => {
                (position as isize + delta).clamp(0, choices.len() as isize - 1) as usize
            }
            None if delta < 0 => choices.len() - 1,
            None => 0,
        };
        self.cursor = Some(choices[position]);
        self.remember();
    }

    fn selection_of(&self, row: Row) -> Selection {
        let worker = &self.entries[row.entry].worker;
        match row.kind {
            RowKind::Computer => Selection::Computer(worker.key.clone()),
            RowKind::AttentionSession => self.session(row).map_or(Selection::None, |session| {
                Selection::Attention(attention::Identity::new(worker, session))
            }),
            RowKind::Session | RowKind::Pinned => match self.session(row) {
                Some(session) => Selection::Session {
                    worker: worker.key.clone(),
                    session: session.id.clone(),
                    pinned: row.kind == RowKind::Pinned,
                },
                None => Selection::None,
            },
            _ => Selection::None,
        }
    }

    fn remember(&mut self) {
        if let Some(row) = self.current_row() {
            self.selection = self.selection_of(row);
        }
    }

    /// Finds the live session a saved pin names: worker key plus session name.
    fn resolve(&self, worker: &str, session: &str) -> Option<(usize, usize)> {
        self.entries.iter().enumerate().find_map(|(i, entry)| {
            if entry.worker.key != worker {
                return None;
            }
            let j = entry.sessions().iter().position(|s| s.name == session)?;
            Some((i, j))
        })
    }

    fn rebuild(&mut self) {
        self.update_attention_order();
        if self.view == View::Attention {
            self.rebuild_attention();
            return;
        }
        let query = caseless::default_case_fold_str(&self.filter);
        let matches = |text: &str| caseless::default_case_fold_str(text).contains(&query);
        let previous = self.cursor;
        self.rows.clear();
        self.cursor = None;
        self.append_attention_rows(AgentState::Waiting, &query, 9);
        if query.is_empty() {
            let mut pinned = Vec::new();
            let mut seen = HashSet::new();
            for pin in &self.state.pins {
                if let Some(found) = self.resolve(&pin.worker, &pin.session)
                    && seen.insert(found)
                {
                    pinned.push(Row::new(RowKind::Pinned, found.0, found.1));
                }
            }
            if !pinned.is_empty() {
                if !self.rows.is_empty() {
                    self.rows.push(Row::new(RowKind::Separator, 0, 0));
                }
                self.rows.push(Row::new(RowKind::PinnedHeader, 0, 0));
                self.rows.extend(pinned);
            }
        }
        // Keep discovery/rescan order within each section. Only availability
        // moves a computer across the divider during background updates.
        for other in [false, true] {
            let mut heading_added = false;
            for (i, entry) in self.entries.iter().enumerate() {
                let available = match entry.status() {
                    Status::Ready => true,
                    Status::Scanning => entry.worker.online != Some(false),
                    _ => false,
                };
                if other == available {
                    continue;
                }
                let worker_match = query.is_empty()
                    || matches(&entry.worker.name)
                    || entry.worker.destination.as_deref().is_some_and(matches);
                let sessions = entry.sessions();
                let mut visible = (0..sessions.len())
                    .filter(|&j| worker_match || matches(&sessions[j].name))
                    .peekable();
                if sessions.is_empty() {
                    if !worker_match {
                        continue;
                    }
                } else if visible.peek().is_none() {
                    continue;
                }
                if !heading_added {
                    if !self.rows.is_empty() {
                        self.rows.push(Row::new(RowKind::Separator, i, 0));
                    }
                    let kind = if other {
                        RowKind::OtherHeader
                    } else {
                        RowKind::ComputersHeader
                    };
                    self.rows.push(Row::new(kind, i, 0));
                    heading_added = true;
                }
                if sessions.is_empty() {
                    self.rows.push(Row::new(RowKind::Computer, i, 0));
                } else {
                    self.rows.push(Row::new(RowKind::Group, i, 0));
                    self.rows
                        .extend(visible.map(|j| Row::new(RowKind::Session, i, j)));
                }
            }
        }
        for (i, row) in self.rows.iter().copied().enumerate() {
            if Self::selectable(row) && self.selection_of(row) == self.selection {
                self.cursor = Some(i);
                return;
            }
        }
        // An inline queue selection carries a session lifetime. Losing that
        // target must not turn Enter into activation of a different session.
        if let Selection::Attention(identity) = &self.selection
            && (!self.filtering || !self.attention_order.contains(identity))
        {
            return;
        }
        // Follow the same computer when sessions arrive or access is lost.
        let replacement = match &self.selection {
            Selection::Computer(worker) => Some((worker, RowKind::Session)),
            Selection::Session { worker, .. } => Some((worker, RowKind::Computer)),
            Selection::None | Selection::Attention(_) => None,
        };
        if let Some((worker, kind)) = replacement
            && let Some(index) = self
                .rows
                .iter()
                .position(|row| row.kind == kind && self.entries[row.entry].worker.key == *worker)
        {
            self.cursor = Some(index);
            // Retain a session's identity across a failed probe.
            if kind == RowKind::Session {
                self.remember();
            }
            return;
        }
        // Keep the remembered identity when a filter temporarily hides it.
        for (i, row) in self.rows.iter().copied().enumerate() {
            if Self::selectable(row) {
                self.cursor = Some(i);
                if previous.is_none_or(|previous| i >= previous) {
                    break;
                }
            }
        }
        if self.selection == Selection::None
            && self
                .current_row()
                .is_some_and(|row| self.entries[row.entry].status() != Status::Scanning)
        {
            self.remember();
        }
    }

    fn activate(&mut self) -> Option<Command> {
        let row = self.current_row()?;
        let entry = &self.entries[row.entry];
        let worker = entry.worker.clone();
        if let Some(session) = self.session(row).cloned() {
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.matches(&worker, Some(&session)))
            {
                self.terminal_focused = true;
                return None;
            }
            self.notice.clear();
            return Some(Command::Launch(Choice {
                worker,
                session: Some(session),
            }));
        }
        if self.status_errors.contains_key(&worker.key) {
            return Some(self.rescan(false));
        }
        let name = display(&worker.name, 80);
        match entry.status() {
            Status::Scanning => self.notice = format!("Still probing {name}…"),
            Status::Ready => {
                self.start_creation();
            }
            _ if worker.destination.is_some() => {
                if self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.matches(&worker, None))
                {
                    self.terminal_focused = true;
                    return None;
                }
                return self.start_user_input(true);
            }
            _ => {
                let detail = entry
                    .inventory
                    .as_ref()
                    .map_or("", |inventory| first_line(&inventory.detail));
                self.notice = format!("{name}: {}", display(detail, 300));
            }
        }
        None
    }

    /// A cached preview, unless a replacement session reused the tmux ID.
    fn cached_preview(&self, key: &PreviewKey, session: &Session) -> Option<&PreviewEntry> {
        self.previews
            .get(key)
            .filter(|entry| same_session(&entry.session, session))
    }
}

impl Drop for Picker {
    fn drop(&mut self) {
        self.cancel.cancel_and_wait();
    }
}

fn preview_refresh(local: bool, sharing: bool, succeeded: bool) -> Duration {
    if succeeded && (local || sharing) {
        PREVIEW_REFRESH
    } else {
        Duration::from_secs(5)
    }
}

fn most_recent(sessions: &[Session]) -> Option<&Session> {
    sessions.iter().max_by_key(|session| session.activity)
}

fn add_computer_unavailable(opts: &crate::options::Options) -> Option<&'static str> {
    if opts.local_only {
        Some(
            "Cannot add a computer with --local: configured hosts are not scanned. Restart without --local.",
        )
    } else if !opts.hosts.is_empty() {
        Some(
            "Cannot add a computer with --host: only the requested hosts are scanned. Restart without --host.",
        )
    } else {
        None
    }
}

fn host_part(destination: &str) -> &str {
    destination
        .rsplit_once('@')
        .map_or(destination, |(_, host)| host)
}

/// OpenSSH prefixes advisory banners (such as the post-quantum key exchange
/// warning) with `** `. They are kept, but after the lines that explain a failure.
fn is_ssh_banner(line: &str) -> bool {
    line.starts_with("** ")
}

/// A diagnostic's non-empty lines, split into messages and OpenSSH banners.
fn diagnostic_lines(text: &str) -> (Vec<&str>, Vec<&str>) {
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .partition(|line| !is_ssh_banner(line.trim_start()))
}

/// The line that best explains a failure: the first message, else a banner.
fn first_line(text: &str) -> &str {
    let (messages, banners) = diagnostic_lines(text);
    messages
        .first()
        .or(banners.first())
        .map_or("", |line| line.trim())
}
