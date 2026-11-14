//! Progressive session picker with an embedded SSH/tmux client.
//! The selector remains available while the client runs in its own PTY.

use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
use std::panic::{self, PanicHookInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    style::Print,
    terminal::{
        BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen,
        LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    AttachOutcome, Choice, Inventory, Status, attach, computer_command, failure, preview, scan,
    switching_local_session,
};
use crate::cancel::Cancel;
use crate::crt::{self, Effects, Screen, Signal};
use crate::display::display;
use crate::embedded::EmbeddedTerminal;
use crate::exit_codes;
use crate::options::Options;
use crate::state::{State, StateChange, default_state_path, load_state, update_state};
use crate::theme::{self, Tint};
use crate::tmux::{INVENTORY_SCRIPT, Session, attach_script, parse_inventory};
use crate::workers::{Config, Worker, discover, load_config, save_host_mapping, with_user};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// A selection must rest this long before its preview is fetched.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(250);
/// A visible preview older than this is refetched while it stays selected.
const PREVIEW_REFRESH: Duration = Duration::from_secs(1);
/// Recent sessions shown in the pinned section, after explicit pins.
const PINNED_RECENTS: usize = 5;
/// How long the preview shows static after the channel changes.
const CHANNEL_STATIC: Duration = Duration::from_millis(150);
// Request wheel/button reports, not the unimplemented all-motion tracking mode.
const ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1006h";
const DISABLE_MOUSE: &str = "\x1b[?1000l\x1b[?1006l";

type Discover = Arc<dyn Fn(&Cancel) -> Result<(Vec<Worker>, String), String> + Send + Sync>;
type Scan = Arc<dyn Fn(&Cancel, Vec<Worker>) -> mpsc::Receiver<Inventory> + Send + Sync>;
type Preview = Arc<
    dyn Fn(&Cancel, &Worker, &Session) -> Result<crate::tmux::Preview, (Status, String)>
        + Send
        + Sync,
>;
type SaveMapping = Box<dyn FnMut(&str, &str) -> Result<(), String>>;
type UpdateState = Box<dyn FnMut(StateChange<'_>) -> Result<State, String>>;

/// Discovery, access checks and persistence, injected for model tests.
struct Services {
    discover: Discover,
    scan: Scan,
    preview: Preview,
    sharing_active: Box<dyn Fn(&str) -> bool>,
    save_mapping: SaveMapping,
    update_state: UpdateState,
    /// Unix seconds, for activity ages and recents.
    now: Box<dyn Fn() -> i64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowKind {
    PinnedHeader,
    Pinned,
    /// A computer's heading above its sessions.
    Group,
    Session,
    Computer,
}

#[derive(Clone, Copy)]
struct Row {
    kind: RowKind,
    entry: usize,
    session: usize,
    /// Pinned rows: a favorite rather than a recent session.
    favorite: bool,
}

impl Row {
    fn new(kind: RowKind, entry: usize, session: usize) -> Self {
        Self {
            kind,
            entry,
            session,
            favorite: false,
        }
    }
}

/// The selection by identity, so it survives row rebuilds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Selection {
    #[default]
    None,
    Computer(String),
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
}

enum Message {
    Discovered {
        generation: u64,
        result: Result<(Vec<Worker>, String), String>,
    },
    Inventory {
        generation: u64,
        inventory: Option<Inventory>,
    },
    Preview {
        generation: u64,
        destination: Option<String>,
        key: PreviewKey,
        session: Session,
        result: Result<crate::tmux::Preview, (Status, String)>,
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
    Discover {
        generation: u64,
        cancel: Cancel,
    },
    Scan {
        generation: u64,
        cancel: Cancel,
        workers: Vec<Worker>,
    },
    Preview {
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
    frame: usize,
    entries: Vec<Entry>,
    warning: String,
    notice: String,
    filter: String,
    filtering: bool,
    input: Option<UserInput>,
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
            frame: 0,
            entries: Vec::new(),
            warning: String::new(),
            notice: String::new(),
            filter: String::new(),
            filtering: false,
            input: None,
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

    fn rescan(&mut self) -> Command {
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
        self.scanning = true;
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
                self.entries = workers
                    .iter()
                    .cloned()
                    .map(|worker| Entry {
                        worker,
                        inventory: None,
                    })
                    .collect();
                self.rebuild();
                if workers.is_empty() {
                    self.scanning = false;
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
                    self.scanning = false;
                    return None;
                };
                if let Some(entry) = self.entries.iter_mut().find(|entry| {
                    entry.worker.key == inventory.worker.key && entry.inventory.is_none()
                }) {
                    entry.inventory = Some(inventory);
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
                return Some(self.rescan());
            }
            Message::Checked(inventory) => {
                if !self.entries.iter().any(|entry| {
                    entry.worker.key == inventory.worker.key
                        && entry.worker.destination == inventory.worker.destination
                }) {
                    self.notice = "Computer configuration changed; rescanning.".into();
                    return Some(self.rescan());
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
                for entry in &mut self.entries {
                    if entry.worker.key == worker
                        && entry.worker.destination == inventory.worker.destination
                    {
                        entry.inventory = Some(inventory.clone());
                    }
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
        self.selection = Selection::Session {
            worker: worker.to_owned(),
            session: session.to_owned(),
            pinned: false,
        };
        self.filter.clear();
        self.filtering = false;
        self.rebuild();
    }

    /// Fetches the selected session's preview once the selection has rested,
    /// and refreshes it while it stays selected. Only one fetch runs at a time.
    fn tick(&mut self, now: Instant) -> Option<Command> {
        self.clock = Some(now);
        self.static_seed = self.static_seed.wrapping_add(1);
        let previewing = self.preview_visible && !self.shows_terminal();
        if previewing && self.selection != self.screen_selection {
            self.screen_selection = self.selection.clone();
            self.static_until = Some(now + CHANNEL_STATIC);
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

    fn key(&mut self, key: KeyEvent) -> Option<Command> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
        {
            self.terminal_focused = false;
            return None;
        }
        if self.terminal_focused {
            if key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.session.is_none())
            {
                return self.finish_terminal(Ok(exit_codes::INTERRUPTED));
            }
            if self.terminal_visible
                && let Some(active) = &mut self.active
                && let Err(error) = active.terminal.send_key(key)
            {
                if error.kind() == io::ErrorKind::WouldBlock {
                    self.notice = format!("Input not sent: {}", display(&error.to_string(), 200));
                    return None;
                }
                return self.finish_terminal(Err(error.to_string()));
            }
            return None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.code = exit_codes::INTERRUPTED;
            self.quit();
            return None;
        }
        if self.input.is_some() {
            return self.input_key(key);
        }
        if self.filtering {
            match key.code {
                KeyCode::Enter => {
                    self.filtering = false;
                    return None;
                }
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.rebuild();
                    return None;
                }
                KeyCode::Backspace => {
                    if self.filter.pop().is_some() {
                        self.rebuild();
                    }
                    return None;
                }
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {}
                _ => {
                    if let KeyCode::Char(ch) = key.code
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        self.filter.push(ch);
                        self.rebuild();
                    }
                    return None;
                }
            }
        }
        // Modified letters are not the corresponding normal-mode shortcuts.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-self.page()),
            KeyCode::PageDown => self.move_cursor(self.page()),
            KeyCode::Home | KeyCode::Char('g') => self.move_cursor(-(self.rows.len() as isize)),
            KeyCode::End | KeyCode::Char('G') => self.move_cursor(self.rows.len() as isize),
            KeyCode::Tab => {
                if self.active.is_some() {
                    self.terminal_focused = true;
                } else if self
                    .current_row()
                    .is_some_and(|row| self.session(row).is_some())
                {
                    return self.activate();
                }
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild();
                }
            }
            KeyCode::Char('f') => self.toggle_pin(),
            KeyCode::Char('u') => self.start_user_input(),
            KeyCode::Char('p') => {
                self.tint = self.tint.next();
                self.notice = format!("Selector tint: {}.", self.tint.label());
            }
            KeyCode::Char('r') => {
                self.notice.clear();
                return Some(self.rescan());
            }
            KeyCode::Char('q') => self.quit(),
            KeyCode::Enter => return self.activate(),
            _ => {}
        }
        None
    }

    fn terminal_mouse_event(&self, mut event: MouseEvent) -> Option<MouseEvent> {
        if !matches!(
            event.kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) || !self.terminal_focused
            || !self.terminal_visible
            || self.active.is_none()
            || self.input.is_some()
            || self.filtering
            || !self
                .terminal_area
                .contains((event.column, event.row).into())
        {
            return None;
        }
        event.column -= self.terminal_area.x;
        event.row -= self.terminal_area.y;
        Some(event)
    }

    fn mouse(&mut self, event: MouseEvent) -> Option<Command> {
        if let Some(event) = self.terminal_mouse_event(event) {
            let active = self.active.as_mut()?;
            match active.terminal.scroll(event) {
                Ok(true) => {}
                Ok(false) => {
                    self.notice.clear();
                    self.notice.push_str(
                        "Scroll: tmux prefix+[ for copy mode, or set -g mouse on on that computer.",
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.notice = format!("Scroll not sent: {}", display(&error.to_string(), 200));
                }
                Err(error) => return self.finish_terminal(Err(error.to_string())),
            }
        } else if !self.terminal_focused
            && self.input.is_none()
            && self.list_area.contains((event.column, event.row).into())
        {
            match event.kind {
                MouseEventKind::ScrollUp => self.move_cursor(-1),
                MouseEventKind::ScrollDown => self.move_cursor(1),
                _ => {}
            }
        }
        None
    }

    fn paste(&mut self, text: &str) -> Option<Command> {
        if self.terminal_focused {
            if self.terminal_visible
                && let Some(active) = &mut self.active
                && let Err(error) = active.terminal.paste(text)
            {
                if error.kind() == io::ErrorKind::WouldBlock {
                    self.notice = format!("Paste not sent: {}", display(&error.to_string(), 200));
                    return None;
                }
                return self.finish_terminal(Err(error.to_string()));
            }
        } else if let Some(input) = &mut self.input {
            input
                .text
                .extend(text.chars().filter(|ch| !ch.is_control()));
        } else if self.filtering {
            self.filter
                .extend(text.chars().filter(|ch| !ch.is_control()));
            self.rebuild();
        }
        None
    }

    fn poll_terminal(&mut self) -> Option<Command> {
        if let Some(error) = self.terminal_error.take() {
            return self.finish_terminal(Err(error));
        }
        let active = self.active.as_mut()?;
        if active.session.is_none() && active.started.elapsed() >= Duration::from_secs(120) {
            return self.finish_terminal(Err("Probe timed out after 120s".into()));
        }
        match active.terminal.poll() {
            Ok(None) => None,
            Ok(Some(code)) => self.finish_terminal(Ok(code)),
            Err(error) => self.finish_terminal(Err(error.to_string())),
        }
    }

    fn finish_terminal(&mut self, result: Result<i32, String>) -> Option<Command> {
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.session.is_some())
        {
            return self.update(Message::Attached(result.map(AttachOutcome::Exited)));
        }
        let active = self.active.take()?;
        if self.terminal_focused {
            self.selection = Selection::Computer(active.worker.key.clone());
        }
        self.terminal_focused = false;
        self.terminal_visible = false;
        self.terminal_error = None;
        if result == Ok(exit_codes::INTERRUPTED) {
            self.notice = format!(
                "Sign-in cancelled for {}.",
                display(&active.worker.name, 80)
            );
            self.rebuild();
            return None;
        }
        let mut inventory = Inventory {
            worker: active.worker,
            sessions: Vec::new(),
            status: Status::Ready,
            detail: String::new(),
        };
        match result {
            Ok(0) => {
                match std::str::from_utf8(active.terminal.captured_output())
                    .map_err(|_| "Probe returned invalid UTF-8".to_owned())
                    .and_then(parse_inventory)
                {
                    Ok(sessions) => inventory.sessions = sessions,
                    Err(error) => {
                        inventory.status = Status::ProbeFailed;
                        inventory.detail = error;
                    }
                }
            }
            Ok(code) => {
                (inventory.status, inventory.detail) = failure(code, &active.terminal.contents());
            }
            Err(error) => {
                inventory.status = Status::ProbeFailed;
                inventory.detail = error;
            }
        }
        self.update(Message::Checked(inventory))
    }

    fn input_key(&mut self, key: KeyEvent) -> Option<Command> {
        let input = self.input.as_mut()?;
        match key.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Backspace => {
                input.text.pop();
            }
            KeyCode::Enter => {
                let input = self.input.take()?;
                let destination = match with_user(&input.destination, input.text.trim()) {
                    Ok(destination) => destination,
                    Err(error) => {
                        self.notice = display(&error, 400);
                        return None;
                    }
                };
                if let Err(error) = (self.services.save_mapping)(&input.key, &destination) {
                    self.notice = format!("Could not save cli.json: {}", display(&error, 400));
                    return None;
                }
                self.notice = format!(
                    "Saved {} → {} in cli.json; rescanning.",
                    display(&input.name, 80),
                    display(&destination, 200)
                );
                return Some(self.rescan());
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && input.text.chars().count() < 64 =>
            {
                input.text.push(ch);
            }
            _ => {}
        }
        None
    }

    fn start_user_input(&mut self) {
        let Some(row) = self.current_row() else {
            return;
        };
        if !matches!(
            row.kind,
            RowKind::Computer | RowKind::Session | RowKind::Pinned
        ) {
            return;
        }
        let worker = &self.entries[row.entry].worker;
        let Some(destination) = &worker.destination else {
            self.notice = "The local computer uses your own account; nothing to map.".into();
            return;
        };
        let current = destination
            .rsplit_once('@')
            .map(|(user, _)| user.to_owned())
            .unwrap_or_default();
        self.input = Some(UserInput {
            key: worker.key.clone(),
            name: worker.name.clone(),
            destination: destination.clone(),
            text: current,
        });
        self.notice.clear();
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
                self.state_error = format!("Could not save pins and recents: {error}");
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
            RowKind::Session | RowKind::Pinned | RowKind::Computer
        )
    }

    fn session(&self, row: Row) -> Option<&Session> {
        if !matches!(row.kind, RowKind::Session | RowKind::Pinned) {
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
        let position = choices
            .iter()
            .position(|i| Some(*i) == self.cursor)
            .unwrap_or(0);
        let position = (position as isize + delta).clamp(0, choices.len() as isize - 1) as usize;
        self.cursor = Some(choices[position]);
        self.remember();
    }

    fn selection_of(&self, row: Row) -> Selection {
        let worker = self.entries[row.entry].worker.key.clone();
        match row.kind {
            RowKind::Computer => Selection::Computer(worker),
            RowKind::Session | RowKind::Pinned => match self.session(row) {
                Some(session) => Selection::Session {
                    worker,
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
        let query = caseless::default_case_fold_str(&self.filter);
        let matches = |text: &str| caseless::default_case_fold_str(text).contains(&query);
        let previous = self.cursor;
        self.rows.clear();
        self.cursor = None;
        if query.is_empty() {
            let mut pinned = Vec::new();
            let mut seen = HashSet::new();
            for pin in &self.state.pins {
                if let Some(found) = self.resolve(&pin.worker, &pin.session)
                    && seen.insert(found)
                {
                    pinned.push(Row {
                        favorite: true,
                        ..Row::new(RowKind::Pinned, found.0, found.1)
                    });
                }
            }
            let mut recents = 0;
            for recent in &self.state.recent {
                if recents == PINNED_RECENTS {
                    break;
                }
                if let Some(found) = self.resolve(&recent.worker, &recent.session)
                    && seen.insert(found)
                {
                    pinned.push(Row::new(RowKind::Pinned, found.0, found.1));
                    recents += 1;
                }
            }
            if !pinned.is_empty() {
                self.rows.push(Row::new(RowKind::PinnedHeader, 0, 0));
                self.rows.extend(pinned);
            }
        }
        let mut computers: Vec<usize> = (0..self.entries.len()).collect();
        computers.sort_by_key(|&i| self.entries[i].rank());
        for i in computers {
            let entry = &self.entries[i];
            let worker_match = query.is_empty()
                || matches(&entry.worker.name)
                || entry.worker.destination.as_deref().is_some_and(matches);
            let sessions = entry.sessions();
            if sessions.is_empty() {
                if worker_match {
                    self.rows.push(Row::new(RowKind::Computer, i, 0));
                }
                continue;
            }
            let mut visible: Vec<usize> = (0..sessions.len())
                .filter(|&j| worker_match || matches(&sessions[j].name))
                .collect();
            if !visible.is_empty() {
                visible.sort_by_key(|&j| std::cmp::Reverse(sessions[j].activity));
                self.rows.push(Row::new(RowKind::Group, i, 0));
                self.rows.extend(
                    visible
                        .into_iter()
                        .map(|j| Row::new(RowKind::Session, i, j)),
                );
            }
        }
        for (i, row) in self.rows.iter().copied().enumerate() {
            if Self::selectable(row) && self.selection_of(row) == self.selection {
                self.cursor = Some(i);
                return;
            }
        }
        // A selected status row becomes a heading when its sessions arrive.
        if let Selection::Computer(worker) = &self.selection
            && let Some(index) = self.rows.iter().position(|row| {
                row.kind == RowKind::Session && self.entries[row.entry].worker.key == *worker
            })
        {
            self.cursor = Some(index);
            self.remember();
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
            self.persist_state(StateChange::RecordRecent {
                worker: &worker.key,
                session: &session.name,
            });
            // The list stays beside the client, so show the new recent now.
            self.remember();
            self.rebuild();
            self.notice.clear();
            return Some(Command::Launch(Choice {
                worker,
                session: Some(session),
            }));
        }
        let name = display(&worker.name, 80);
        match entry.status() {
            Status::Scanning => self.notice = format!("Still probing {name}…"),
            Status::Ready => {
                self.notice = format!(
                    "{name} has no tmux sessions yet; start one there with `tmux new -s NAME`."
                );
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
                self.notice.clear();
                return Some(Command::Launch(Choice {
                    worker,
                    session: None,
                }));
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

    fn counts(&self) -> (usize, usize, usize) {
        let sessions = self
            .entries
            .iter()
            .map(|entry| entry.sessions().len())
            .sum();
        let setup = self
            .entries
            .iter()
            .filter(|entry| entry.needs_setup())
            .count();
        (sessions, self.entries.len(), setup)
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let tint = self.tint;
        self.terminal_visible = false;
        self.list_area = Rect::default();
        if self.active.is_some() && self.terminal_focused && area.height < 5 {
            self.preview_visible = false;
            self.list_height = 0;
            self.render_terminal(frame, area);
            return;
        }
        let mut status_lines: Vec<Line<'static>> = Vec::new();
        if let Some(input) = &self.input {
            status_lines.push(Line::styled(
                format!(
                    "SSH user for {}: {}▏  (saved as user@{} in cli.json)",
                    display(&input.name, 80),
                    display(&input.text, 64),
                    display(host_part(&input.destination), 120)
                ),
                tint.accent(),
            ));
        }
        if self.filtering || !self.filter.is_empty() {
            status_lines.push(Line::from(vec![
                Span::styled("/", tint.accent()),
                Span::raw(display(&self.filter, 200)),
                Span::styled(if self.filtering { "▏" } else { "" }, tint.accent()),
            ]));
        }
        for warning in [&self.warning, &self.state_error] {
            if !warning.is_empty() {
                status_lines.push(Line::styled(
                    format!("! {}", display(warning, 600)),
                    theme::warning(),
                ));
            }
        }
        if !self.notice.is_empty() {
            status_lines.push(Line::styled(self.notice.clone(), theme::warning()));
        }
        let [header, body, status, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length((status_lines.len() as u16).min(area.height.saturating_sub(5))),
            Constraint::Length(1),
        ])
        .areas(area);
        let (sessions, computers, setup) = self.counts();
        frame.render_widget(
            Paragraph::new(self.header_line(sessions, computers, setup)),
            header,
        );
        frame.render_widget(Paragraph::new(status_lines), status);
        frame.render_widget(Paragraph::new(self.footer_line()), footer);

        // Keep the sidebar stable through scans, filtering, and selection changes.
        // At 80 columns it leaves 48 for the preview or terminal.
        let compact = body.width >= 80 || body.height >= 24;
        let width = 32;
        let beside = compact && body.width.saturating_sub(width) >= 48;
        let (list_area, preview_area) = if self.active.is_some()
            && self.terminal_focused
            && (!beside || body.height < 3)
            && body.height < 24
        {
            (Rect::default(), Some(body))
        } else if beside {
            let [list, preview] =
                Layout::horizontal([Constraint::Length(width), Constraint::Min(48)]).areas(body);
            (list, Some(preview))
        } else if body.height >= 24 {
            // Fit short lists; long lists scroll within half the available height.
            // A live client keeps its size while scans and filters change the rows.
            let half = body.height / 2;
            let list_height = if self.active.is_some() {
                half
            } else {
                self.rows
                    .len()
                    .max(1)
                    .saturating_add(2)
                    .min(usize::from(half)) as u16
            };
            let [list, preview] =
                Layout::vertical([Constraint::Length(list_height), Constraint::Min(10)])
                    .areas(body);
            (list, Some(preview))
        } else {
            (body, None)
        };
        let show_terminal = self.shows_terminal();
        self.preview_visible = !show_terminal && preview_area.is_some();
        self.list_height = list_area.height;

        let items = self.render_rows(compact, list_area.width);
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(if self.terminal_focused {
                theme::muted()
            } else {
                tint.border()
            })
            .title(Line::styled(
                format!(" Computers · {computers} "),
                tint.heading(),
            ));
        if let Some(background) = tint.background() {
            block = block.style(Style::default().bg(background));
        }
        let items = if !items.is_empty() {
            items
        } else {
            vec![ListItem::new(Line::styled(
                self.empty_message(),
                tint.secondary(),
            ))]
        };
        self.list_area = block.inner(list_area);
        self.list_state.select(self.cursor);
        let list = List::new(items)
            .block(block)
            .highlight_symbol("❯ ")
            .highlight_style(tint.selected());
        frame.render_stateful_widget(list, list_area, &mut self.list_state);
        if let Some(area) = preview_area {
            if show_terminal {
                self.render_terminal(frame, area);
            } else {
                self.terminal_area = Block::bordered().inner(area);
                self.render_preview(frame, area);
            }
        }
    }

    fn render_terminal(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(active) = &mut self.active else {
            return;
        };
        let title = match &active.session {
            Some(session) => format!(
                " {} / {} · {} ",
                display(&active.worker.name, 80),
                display(&session.name, 80),
                if self.terminal_focused {
                    "terminal"
                } else {
                    "connected"
                },
            ),
            None => format!(" {} · sign in ", display(&active.worker.name, 80)),
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(if self.terminal_focused {
                self.tint.accent()
            } else {
                theme::muted()
            })
            .title(Line::styled(title, self.tint.heading()));
        let content = if area.width < 3 || area.height < 3 {
            area
        } else {
            let content = block.inner(area);
            frame.render_widget(block, area);
            content
        };
        self.terminal_area = content;
        self.terminal_visible = content.width > 0 && content.height > 0;
        if !self.terminal_visible {
            return;
        }
        if let Err(error) = active.terminal.resize(content.height, content.width) {
            self.terminal_error = Some(error.to_string());
            self.terminal_visible = false;
            return;
        }
        active
            .terminal
            .render(frame, content, self.terminal_focused);
    }

    fn header_line(&self, sessions: usize, computers: usize, setup: usize) -> Line<'static> {
        let mut spans = vec![
            Span::styled(" delux", self.tint.accent()),
            Span::styled(
                format!(
                    "  {} · {}",
                    plural(sessions as i64, "session"),
                    plural(computers as i64, "computer")
                ),
                theme::muted(),
            ),
        ];
        if setup > 0 {
            spans.push(Span::styled(
                format!(" · {setup} need{} setup", if setup == 1 { "s" } else { "" }),
                theme::warning(),
            ));
        }
        if self.scanning {
            let probed = self
                .entries
                .iter()
                .filter(|entry| entry.inventory.is_some())
                .count();
            spans.push(Span::styled(
                format!(
                    " · {} probing {probed}/{}",
                    SPINNER[self.frame],
                    self.entries.len()
                ),
                theme::muted(),
            ));
        }
        Line::from(spans)
    }

    fn footer_line(&self) -> Line<'static> {
        let hints: &[(&str, &str)] = if self.terminal_focused
            && self
                .active
                .as_ref()
                .is_some_and(|active| active.session.is_none())
        {
            &[
                ("shift-tab", "list · sign-in focused"),
                ("ctrl-c", "cancel sign-in"),
            ]
        } else if self.terminal_focused {
            &[
                ("shift-tab", "list · terminal focused"),
                ("tmux prefix d", "detach"),
            ]
        } else if self.input.is_some() {
            &[("enter", "save"), ("esc", "cancel")]
        } else if self.filtering {
            &[
                ("type", "to filter"),
                ("↑↓", "move"),
                ("enter", "done"),
                ("esc", "clear"),
            ]
        } else if self
            .current_row()
            .is_some_and(|row| row.kind == RowKind::Computer)
        {
            &[
                ("↑↓", "move"),
                ("enter", "connect"),
                ("u", "ssh user"),
                ("/", "filter"),
                ("p", "tint"),
                ("r", "rescan"),
                ("q", "quit"),
            ]
        } else {
            &[
                ("↑↓", "move"),
                (
                    if self.active.is_some() {
                        "enter"
                    } else {
                        "enter/tab"
                    },
                    "open",
                ),
                ("/", "filter"),
                ("f", "pin"),
                ("u", "ssh user"),
                ("p", "tint"),
                ("r", "rescan"),
                ("q", "quit"),
            ]
        };
        let mut spans = vec![Span::raw(" ")];
        if self.active.is_some()
            && !self.terminal_focused
            && self.input.is_none()
            && !self.filtering
        {
            spans.push(Span::styled("tab", self.tint.accent()));
            spans.push(Span::styled(" terminal · list focused  ", theme::muted()));
        }
        for (i, (key, description)) in hints.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(*key, self.tint.accent()));
            spans.push(Span::styled(format!(" {description}"), theme::muted()));
        }
        Line::from(spans)
    }

    fn empty_message(&self) -> String {
        if !self.filter.is_empty() {
            "Nothing matches the filter.".into()
        } else if self.scanning {
            "Discovering computers…".into()
        } else {
            "No computers found.".into()
        }
    }

    fn render_preview(&self, frame: &mut Frame<'_>, area: Rect) {
        let static_seed = match (self.static_until, self.clock) {
            (Some(until), Some(now)) if now < until => Some(self.static_seed),
            _ => None,
        };
        crt::render(frame, area, &self.screen(), Effects { static_seed });
    }

    /// A cached preview, unless a replacement session reused the tmux ID.
    fn cached_preview(&self, key: &PreviewKey, session: &Session) -> Option<&PreviewEntry> {
        self.previews
            .get(key)
            .filter(|entry| same_session(&entry.session, session))
    }

    /// What the preview shows for the current selection.
    fn screen(&self) -> Screen {
        let Some(row) = self.current_row() else {
            return Screen {
                channel: None,
                label: "standby".into(),
                detail: String::new(),
                lines: vec![Line::styled("Select a session to tune in.", theme::muted())],
                signal: Signal::Standby,
            };
        };
        let entry = &self.entries[row.entry];
        if let Some(session) = self.session(row) {
            let key = (entry.worker.key.clone(), session.id.clone());
            let updating = self
                .preview_inflight
                .as_ref()
                .is_some_and(|(inflight, _)| *inflight == key);
            // Channels number the session rows from the top of the list.
            let channel = self.rows[..=self.cursor.unwrap_or_default()]
                .iter()
                .filter(|row| matches!(row.kind, RowKind::Session | RowKind::Pinned))
                .count();
            let cached = self
                .cached_preview(&key, session)
                .map(|entry| &entry.result);
            let (lines, signal) = match cached {
                Some(Ok(text)) => (
                    crate::ansi::styled_lines(text),
                    if updating {
                        Signal::Updating
                    } else {
                        Signal::Live
                    },
                ),
                Some(Err(error)) => (
                    vec![Line::styled(display(error, 400), theme::error())],
                    Signal::Lost,
                ),
                None => (Vec::new(), Signal::Tuning),
            };
            return Screen {
                channel: Some(channel),
                label: format!("{} / {}", entry.worker.name, session.name),
                detail: session_details(session, (self.services.now)()),
                lines,
                signal,
            };
        }
        self.computer_screen(entry)
    }

    fn computer_screen(&self, entry: &Entry) -> Screen {
        let status = entry.status();
        let label = format!("{} · {status}", entry.worker.name);
        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(destination) = &entry.worker.destination {
            lines.push(Line::styled(
                format!("ssh {}", display(destination, 200)),
                theme::muted(),
            ));
            lines.push(Line::raw(""));
        }
        let signal = match status {
            Status::Scanning => Signal::Tuning,
            Status::Ready => {
                lines.push(Line::raw("No tmux sessions on this computer yet."));
                lines.push(Line::styled(
                    "Start one there with `tmux new -s NAME`, then press r.",
                    theme::muted(),
                ));
                Signal::Ready
            }
            _ => {
                if let Some(inventory) = &entry.inventory {
                    let (messages, banners) = diagnostic_lines(&inventory.detail);
                    lines.extend(
                        messages
                            .into_iter()
                            .map(|line| Line::raw(display(line, 400))),
                    );
                    lines.extend(
                        banners
                            .into_iter()
                            .map(|line| Line::styled(display(line, 400), theme::muted())),
                    );
                }
                let hint = access_hint(status, entry.worker.destination.is_some());
                if !hint.is_empty() {
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(hint, theme::muted()));
                }
                Signal::Lost
            }
        };
        Screen {
            channel: None,
            label,
            detail: String::new(),
            lines,
            signal,
        }
    }

    /// Rows are compact (names, plus a short activity age) when the preview is
    /// shown, because the preview carries each session's details.
    fn render_rows(&self, compact: bool, width: u16) -> Vec<ListItem<'static>> {
        let content_width = usize::from(width.saturating_sub(4));
        let name_width = if compact {
            content_width.saturating_sub(8)
        } else {
            48
        };
        let tint = self.tint;
        let now = (self.services.now)();
        let details = |session: &Session| {
            if compact {
                format!("{:>4}", short_age(now, session.output))
            } else {
                session_details(session, now)
            }
        };
        // Pad session names within each computer, and pins as their own group.
        let group = |row: Row| match row.kind {
            RowKind::Pinned => usize::MAX,
            _ => row.entry,
        };
        // Pinned rows also name their computer. It shares the room beside the
        // mark and age (or details) with the session name, taking at most half.
        let pinned_room = content_width.saturating_sub(if compact { 10 } else { 7 });
        let host_width = self
            .rows
            .iter()
            .filter(|row| row.kind == RowKind::Pinned)
            .map(|row| {
                let host = fit_name(&self.entries[row.entry].worker.name, pinned_room);
                UnicodeWidthStr::width(host.as_str())
            })
            .max()
            .unwrap_or(0)
            .min(pinned_room / 2);
        let pinned_name_width = name_width.min(pinned_room - host_width);
        let name_of = |row: Row| {
            let width = if row.kind == RowKind::Pinned {
                pinned_name_width
            } else {
                name_width
            };
            self.session(row)
                .map(|session| fit_name(&session.name, width))
        };
        let mut widths: HashMap<usize, usize> = HashMap::new();
        for row in self.rows.iter().copied() {
            if let Some(name) = name_of(row) {
                let current = widths.entry(group(row)).or_default();
                *current = (*current).max(UnicodeWidthStr::width(name.as_str()));
            }
        }
        let padded = |row: Row| {
            let name = name_of(row).unwrap_or_default();
            let width = widths.get(&group(row)).copied().unwrap_or(0);
            let padding = width.saturating_sub(UnicodeWidthStr::width(name.as_str()));
            format!("{name}{}", " ".repeat(padding))
        };
        self.rows
            .iter()
            .copied()
            .map(|row| {
                let entry = &self.entries[row.entry];
                let spans = match row.kind {
                    RowKind::PinnedHeader => vec![Span::styled("Pinned", tint.heading())],
                    RowKind::Group => {
                        let mut spans = vec![Span::styled(
                            fit_name(&entry.worker.name, content_width),
                            tint.heading(),
                        )];
                        if let Some(destination) = &entry.worker.destination
                            && !compact
                        {
                            spans.push(Span::styled(
                                format!("  {}", display(destination, 120)),
                                tint.secondary(),
                            ));
                        }
                        spans
                    }
                    RowKind::Pinned => {
                        let mark = if row.favorite { "★ " } else { "↺ " };
                        let mut spans = vec![
                            Span::styled(mark, tint.heading()),
                            Span::styled(padded(row), tint.text()),
                        ];
                        let detail = self.session(row).map(|session| {
                            Span::styled(format!("  {}", details(session)), tint.secondary())
                        });
                        let host = fit_name(&entry.worker.name, host_width);
                        if compact {
                            spans.extend(detail);
                            spans.push(Span::styled(format!("  {host}"), tint.secondary()));
                        } else {
                            // Full details vary in length, so the computer comes first.
                            let padding =
                                host_width.saturating_sub(UnicodeWidthStr::width(host.as_str()));
                            spans.push(Span::styled(
                                format!("  on {host}{}", " ".repeat(padding)),
                                tint.secondary(),
                            ));
                            spans.extend(detail);
                        }
                        spans
                    }
                    RowKind::Session => {
                        let session = self.session(row);
                        let pinned = session.is_some_and(|session| {
                            self.state.is_pinned(&entry.worker.key, &session.name)
                        });
                        let mut spans = vec![
                            Span::styled(if pinned { "★ " } else { "  " }, tint.heading()),
                            Span::styled(padded(row), tint.text()),
                        ];
                        if let Some(session) = session {
                            spans.push(Span::styled(
                                format!("  {}", details(session)),
                                tint.secondary(),
                            ));
                        }
                        spans
                    }
                    RowKind::Computer => {
                        let status = entry.status();
                        let offline = entry.worker.online == Some(false);
                        let dot = if offline {
                            "○"
                        } else if status == Status::Scanning {
                            SPINNER[self.frame]
                        } else {
                            "●"
                        };
                        let status_text = if offline {
                            "offline"
                        } else {
                            match status {
                                Status::Scanning => "probing…",
                                Status::Ready => "no sessions",
                                Status::AuthRequired => "Sign in",
                                Status::HostTrust => "Trust key",
                                Status::MissingTmux => "no tmux",
                                Status::MissingTools => "missing tools",
                                Status::ProbeFailed => "probe failed",
                                Status::Unreachable => "unreachable",
                            }
                        };
                        let style = if offline {
                            theme::muted()
                        } else {
                            theme::status(status)
                        };
                        let name_width =
                            content_width.saturating_sub(UnicodeWidthStr::width(status_text) + 4);
                        vec![
                            Span::styled(fit_name(&entry.worker.name, name_width), tint.heading()),
                            Span::styled(format!("  {dot} "), style),
                            Span::styled(status_text, style),
                        ]
                    }
                };
                ListItem::new(Line::from(spans))
            })
            .collect()
    }
}

/// Keep the action or age visible even beside wide Unicode names.
fn fit_name(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut name = display(value, width);
    let mut used = 0;
    let mut end = 0;
    for (index, character) in name.char_indices() {
        used += character.width().unwrap_or(0);
        if used > width {
            name.truncate(end);
            name.push('…');
            break;
        }
        if used < width {
            end = index + character.len_utf8();
        }
    }
    name
}

impl Drop for Picker {
    fn drop(&mut self) {
        self.cancel.cancel_and_wait();
    }
}

/// The active pane's command, its directory, activity age, windows and clients.
fn session_details(session: &Session, now: i64) -> String {
    let mut details = Vec::new();
    if !session.command.is_empty() {
        details.push(display(&session.command, 40));
    }
    if let Some(directory) = directory_name(&session.path) {
        details.push(display(directory, 60));
    }
    details.push(format!("output {}", age(now, session.output)));
    details.push(format!("used {}", age(now, session.activity)));
    if session.windows > 1 {
        details.push(plural(session.windows, "window"));
    }
    details.push(if session.attached > 0 {
        format!("{} attached", session.attached)
    } else {
        "detached".to_owned()
    });
    details.join(" · ")
}

/// The last component of a pane's working directory.
fn directory_name(path: &str) -> Option<&str> {
    if path.is_empty() {
        return None;
    }
    Some(
        path.rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or("/"),
    )
}

/// A compact relative age, e.g. "now", "45s ago", "12m ago", "3h ago", "2d ago".
fn age(now: i64, then: i64) -> String {
    match short_age(now, then).as_str() {
        "now" => "now".to_owned(),
        short => format!("{short} ago"),
    }
}

/// The shortest relative age, for list rows: "now", "45s", "12m", "3h", "2d".
fn short_age(now: i64, then: i64) -> String {
    let seconds = now.saturating_sub(then).max(0);
    match seconds {
        0..5 => "now".to_owned(),
        5..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", (seconds / 86_400).min(999)),
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

fn access_hint(status: Status, remote: bool) -> &'static str {
    match status {
        Status::AuthRequired if remote => {
            "Wrong user or key? Press u to set the SSH user, or set User/IdentityFile in ~/.ssh/config; enter connects interactively."
        }
        Status::HostTrust if remote => {
            "Press enter to connect interactively and verify the host key."
        }
        _ if remote => "Press enter to try connecting interactively, or r to rescan.",
        _ => "Press r to rescan.",
    }
}

fn plural(count: i64, word: &str) -> String {
    format!("{count} {word}{}", if count == 1 { "" } else { "s" })
}

type PanicHook = Arc<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

struct TerminalGuard {
    active: Arc<AtomicBool>,
    previous_hook: PanicHook,
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        EndSynchronizedUpdate,
        DisableBracketedPaste,
        Print(DISABLE_MOUSE),
        LeaveAlternateScreen,
        Show
    );
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let active = Arc::new(AtomicBool::new(false));
        let previous_hook: PanicHook = Arc::from(panic::take_hook());
        let hook_active = active.clone();
        let prior = previous_hook.clone();
        panic::set_hook(Box::new(move |info| {
            hook_active.store(false, Ordering::SeqCst);
            restore_terminal();
            prior(info);
        }));
        let guard = Self {
            active,
            previous_hook,
        };
        guard.resume()?;
        Ok(guard)
    }

    fn suspend(&self) -> io::Result<()> {
        // Attempt every restoration even if an earlier operation fails.
        let raw = disable_raw_mode();
        let screen = execute!(
            io::stdout(),
            EndSynchronizedUpdate,
            DisableBracketedPaste,
            Print(DISABLE_MOUSE),
            LeaveAlternateScreen,
            Show
        );
        raw?;
        screen?;
        self.active.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn resume(&self) -> io::Result<()> {
        // Mark active before setup so partial setup failures are also restored.
        self.active.store(true, Ordering::SeqCst);
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Print(ENABLE_MOUSE),
            Hide
        )
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active.swap(false, Ordering::SeqCst) {
            restore_terminal();
        }
        if !std::thread::panicking() {
            let previous = self.previous_hook.clone();
            panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

/// Clears the screen and returns a terminal whose empty buffers force a full
/// redraw. `Terminal::clear` would query the cursor position first, which
/// fails after two seconds on terminals that do not answer that query.
fn fresh_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    execute!(io::stdout(), Clear(ClearType::All))?;
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

fn execute_command(
    command: Command,
    picker: &mut Picker,
    sender: &mpsc::Sender<Message>,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    guard: &TerminalGuard,
) -> Result<(), String> {
    match command {
        Command::Discover { generation, cancel } => {
            let discover = picker.services.discover.clone();
            let sender = sender.clone();
            std::thread::spawn(move || {
                let result = discover(&cancel);
                let _ = sender.send(Message::Discovered { generation, result });
            });
        }
        Command::Scan {
            generation,
            cancel,
            workers,
        } => {
            let scan = picker.services.scan.clone();
            let sender = sender.clone();
            std::thread::spawn(move || {
                for inventory in scan(&cancel, workers) {
                    if sender
                        .send(Message::Inventory {
                            generation,
                            inventory: Some(inventory),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                let _ = sender.send(Message::Inventory {
                    generation,
                    inventory: None,
                });
            });
        }
        Command::Preview {
            worker,
            session,
            cancel,
        } => {
            let preview = picker.services.preview.clone();
            let sender = sender.clone();
            let generation = picker.generation;
            std::thread::spawn(move || {
                let result = preview(&cancel, &worker, &session);
                if !cancel.is_cancelled() {
                    let key = (worker.key, session.id.clone());
                    let _ = sender.send(Message::Preview {
                        generation,
                        destination: worker.destination,
                        key,
                        session,
                        result,
                    });
                }
            });
        }
        Command::Launch(choice) => {
            if let Some(session) = &choice.session {
                // Nesting the local session containing Delux would display itself.
                // Preserve tmux's native client switch when already on that server.
                match switching_local_session(&choice.worker, session) {
                    Ok(true) => {
                        picker.active = None;
                        guard.suspend().map_err(|error| error.to_string())?;
                        let result = attach(&picker.cancel, &choice.worker, session);
                        let next = picker.update(Message::Attached(result));
                        if !picker.quit {
                            guard.resume().map_err(|error| error.to_string())?;
                            *terminal = fresh_terminal().map_err(|error| error.to_string())?;
                            if let Some(next) = next {
                                execute_command(next, picker, sender, terminal, guard)?;
                            }
                        }
                        return Ok(());
                    }
                    Err(error) => {
                        picker.notice = format!("Attach failed: {}", display(&error, 400));
                        return Ok(());
                    }
                    Ok(false) => {}
                }
            }
            // Drop only the previous client, never its server or the SSH master.
            picker.active = None;
            picker.terminal_focused = false;
            picker.terminal_visible = false;
            if let Some((_, cancel)) = picker.preview_inflight.take() {
                cancel.cancel();
            }
            picker.preview_target = None;
            let attachment = crate::ssh_mux::begin_attachment();
            let signing_in = choice.session.is_none();
            let args = match &choice.session {
                Some(session) => attach_script(session)
                    .map(|script| computer_command(&choice.worker, &script, true, true)),
                None => Ok(computer_command(
                    &choice.worker,
                    INVENTORY_SCRIPT,
                    true,
                    false,
                )),
            };
            let result = args.and_then(|args| {
                let spawn = if signing_in {
                    EmbeddedTerminal::spawn_captured
                } else {
                    EmbeddedTerminal::spawn
                };
                spawn(
                    &args,
                    picker.terminal_area.height.max(1),
                    picker.terminal_area.width.max(1),
                )
                .map_err(|error| error.to_string())
            });
            match result {
                Ok(terminal) => {
                    picker.active = Some(ActiveTerminal {
                        worker: choice.worker,
                        session: choice.session,
                        started: Instant::now(),
                        terminal,
                        _attachment: attachment,
                    });
                    picker.terminal_focused = true;
                    if signing_in {
                        picker.notice =
                            "Verify any host fingerprint through a trusted channel.".into();
                    }
                }
                Err(error) => {
                    let message = if signing_in {
                        Message::Checked(Inventory {
                            worker: choice.worker,
                            sessions: Vec::new(),
                            status: Status::ProbeFailed,
                            detail: error,
                        })
                    } else {
                        Message::Attached(Err(error))
                    };
                    if let Some(next) = picker.update(message) {
                        execute_command(next, picker, sender, terminal, guard)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Runs the picker. Each scan re-reads the configuration at `opts.config_path`
/// so mappings saved from the picker apply to the next scan.
pub fn run_picker(cancel: &Cancel, opts: &Options) -> Result<i32, String> {
    let discover_opts = opts.clone();
    let config_path = opts.config_path.clone();
    let state_path = default_state_path();
    let state = state_path.clone().and_then(|path| load_state(&path));
    let services = Services {
        discover: Arc::new(move |cancel| {
            let config: Config = load_config(&discover_opts.config_path)?;
            discover(cancel, &config, &discover_opts)
        }),
        scan: Arc::new(scan),
        preview: Arc::new(preview),
        sharing_active: Box::new(crate::ssh_mux::sharing_active),
        save_mapping: Box::new(move |name, destination| {
            save_host_mapping(&config_path, name, destination)
        }),
        update_state: Box::new(move |change| match &state_path {
            Ok(path) => update_state(path, change),
            Err(error) => Err(error.clone()),
        }),
        now: Box::new(unix_now),
    };
    let mut picker = Picker::new(cancel.clone(), services, state);
    let guard = TerminalGuard::enter().map_err(|error| error.to_string())?;
    let mut terminal = fresh_terminal().map_err(|error| error.to_string())?;
    let (sender, receiver) = mpsc::channel();
    let initial = picker.rescan();
    execute_command(initial, &mut picker, &sender, &mut terminal, &guard)?;
    let mut tick = Instant::now();
    loop {
        if cancel.is_cancelled() {
            return Ok(exit_codes::INTERRUPTED);
        }
        if let Some(command) = picker.poll_terminal() {
            execute_command(command, &mut picker, &sender, &mut terminal, &guard)?;
        }
        while let Ok(message) = receiver.try_recv() {
            if let Some(command) = picker.update(message) {
                execute_command(command, &mut picker, &sender, &mut terminal, &guard)?;
            }
            if picker.quit {
                break;
            }
        }
        if picker.quit {
            return match picker.error.take() {
                Some(error) => Err(error),
                None => Ok(picker.code),
            };
        }
        if tick.elapsed() >= Duration::from_millis(120) {
            if picker.scanning || picker.entries.iter().any(|entry| entry.inventory.is_none()) {
                picker.update(Message::Spinner);
            }
            tick = Instant::now();
        }
        // Tick first so a channel change shows static on the very next frame.
        if let Some(command) = picker.update(Message::Tick(Instant::now())) {
            execute_command(command, &mut picker, &sender, &mut terminal, &guard)?;
        }
        // Present the cells and final cursor together, not the intermediate
        // paint cursor that Crossterm flushes before restoring its position.
        execute!(terminal.backend_mut(), BeginSynchronizedUpdate)
            .map_err(|error| error.to_string())?;
        let drawn = terminal.draw(|frame| picker.render(frame)).map(|_| ());
        let presented = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
        drawn.and(presented).map_err(|error| error.to_string())?;
        let wait = Duration::from_millis(if picker.active.is_some() { 16 } else { 100 })
            .min(Duration::from_millis(120).saturating_sub(tick.elapsed()));
        if event::poll(wait).map_err(|error| error.to_string())? {
            let command = match event::read().map_err(|error| error.to_string())? {
                Event::Key(key) => picker.update(Message::Key(key)),
                Event::Mouse(event) => picker.update(Message::Mouse(event)),
                Event::Paste(text) => picker.update(Message::Paste(text)),
                _ => None,
            };
            if let Some(command) = command {
                execute_command(command, &mut picker, &sender, &mut terminal, &guard)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Pin;
    use crate::test_support::{EnvVar, env_write};
    use crate::workers::local_worker;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;
    use std::cell::RefCell;
    use std::rc::Rc;

    const NOW: i64 = 1_000_000;

    fn worker(key: &str, name: &str) -> Worker {
        Worker {
            key: key.into(),
            name: name.into(),
            destination: None,
            online: None,
        }
    }

    fn remote(key: &str, name: &str, destination: &str) -> Worker {
        Worker {
            destination: Some(destination.into()),
            ..worker(key, name)
        }
    }

    fn session(id: &str, name: &str) -> Session {
        Session {
            id: id.into(),
            name: name.into(),
            windows: 1,
            attached: 0,
            server_pid: 1,
            server_start: 1,
            socket_path: "/tmp/tmux-test".into(),
            created: 1,
            activity: 1,
            output: 1,
            command: String::new(),
            path: String::new(),
        }
    }

    fn ready(worker: &Worker, sessions: Vec<Session>) -> Inventory {
        Inventory {
            worker: worker.clone(),
            sessions,
            status: Status::Ready,
            detail: String::new(),
        }
    }

    fn failed(worker: &Worker, status: Status, detail: &str) -> Inventory {
        Inventory {
            worker: worker.clone(),
            sessions: vec![],
            status,
            detail: detail.into(),
        }
    }

    /// Observable effects of a picker under test.
    #[derive(Default)]
    struct Effects {
        launched: Vec<Choice>,
        mappings: Vec<(String, String)>,
        states: Vec<State>,
        previews: Vec<PreviewKey>,
    }

    struct Harness {
        picker: Picker,
        effects: Rc<RefCell<Effects>>,
    }

    impl Harness {
        fn new(workers: Vec<Worker>, inventories: Vec<Inventory>) -> Self {
            Self::with_state(workers, inventories, State::default())
        }

        fn with_state(workers: Vec<Worker>, inventories: Vec<Inventory>, state: State) -> Self {
            Self::with_loaded_state(workers, inventories, Ok(state))
        }

        fn with_loaded_state(
            workers: Vec<Worker>,
            inventories: Vec<Inventory>,
            state: Result<State, String>,
        ) -> Self {
            let effects = Rc::new(RefCell::new(Effects::default()));
            let (mappings, states) = (effects.clone(), effects.clone());
            let mut saved_state = state.as_ref().cloned().unwrap_or_default();
            let services = Services {
                discover: Arc::new(move |_| Ok((workers.clone(), String::new()))),
                scan: Arc::new(move |_, _| {
                    let (sender, receiver) = mpsc::channel();
                    for inventory in &inventories {
                        sender.send(inventory.clone()).unwrap();
                    }
                    receiver
                }),
                preview: Arc::new(|_, worker, session| {
                    Ok(crate::tmux::Preview {
                        text: format!(
                            "output of {} on {}\n\tindented\x1b[2J",
                            session.name, worker.name
                        ),
                        activity: session.activity,
                        output: session.output,
                    })
                }),
                sharing_active: Box::new(|_| true),
                save_mapping: Box::new(move |name, destination| {
                    mappings
                        .borrow_mut()
                        .mappings
                        .push((name.into(), destination.into()));
                    Ok(())
                }),
                update_state: Box::new(move |change| {
                    saved_state.apply(change);
                    states.borrow_mut().states.push(saved_state.clone());
                    Ok(saved_state.clone())
                }),
                now: Box::new(|| NOW),
            };
            let mut picker = Picker::new(Cancel::new(), services, state);
            let initial = picker.rescan();
            let mut harness = Self { picker, effects };
            harness.drive(Some(initial));
            assert!(!harness.picker.scanning);
            harness
        }

        fn drive(&mut self, command: Option<Command>) {
            let Some(command) = command else {
                return;
            };
            let next = match command {
                Command::Discover { generation, cancel } => {
                    let result = (self.picker.services.discover)(&cancel);
                    self.picker
                        .update(Message::Discovered { generation, result })
                }
                Command::Scan {
                    generation,
                    cancel,
                    workers,
                } => {
                    for inventory in (self.picker.services.scan)(&cancel, workers) {
                        let next = self.picker.update(Message::Inventory {
                            generation,
                            inventory: Some(inventory),
                        });
                        self.drive(next);
                    }
                    self.picker.update(Message::Inventory {
                        generation,
                        inventory: None,
                    })
                }
                Command::Preview {
                    worker,
                    session,
                    cancel,
                } => {
                    let key = (worker.key.clone(), session.id.clone());
                    self.effects.borrow_mut().previews.push(key.clone());
                    let result = (self.picker.services.preview)(&cancel, &worker, &session);
                    self.picker.update(Message::Preview {
                        generation: self.picker.generation,
                        destination: worker.destination,
                        key,
                        session,
                        result,
                    })
                }
                Command::Launch(choice) => {
                    self.effects.borrow_mut().launched.push(choice);
                    None
                }
            };
            self.drive(next);
        }

        fn press(&mut self, keys: &[&str]) -> bool {
            for key in keys {
                let codes = match *key {
                    "enter" => vec![KeyCode::Enter],
                    "esc" => vec![KeyCode::Esc],
                    "down" => vec![KeyCode::Down],
                    "up" => vec![KeyCode::Up],
                    "tab" => vec![KeyCode::Tab],
                    "backspace" => vec![KeyCode::Backspace],
                    text => text.chars().map(KeyCode::Char).collect(),
                };
                for code in codes {
                    let next = self
                        .picker
                        .update(Message::Key(KeyEvent::new(code, KeyModifiers::NONE)));
                    self.drive(next);
                }
            }
            self.picker.quit
        }

        fn tick(&mut self, at: Instant) {
            let next = self.picker.update(Message::Tick(at));
            self.drive(next);
        }

        fn buffer(&mut self, width: u16, height: u16) -> Buffer {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| self.picker.render(frame)).unwrap();
            terminal.backend().buffer().clone()
        }

        fn rendered(&mut self, width: u16, height: u16) -> String {
            let buffer = self.buffer(width, height);
            (0..height)
                .map(|y| {
                    (0..width)
                        .map(|x| buffer[(x, y)].symbol().to_owned())
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn launched(&self) -> Vec<Choice> {
            self.effects.borrow().launched.clone()
        }
    }

    /// A real PTY child exercises panel behavior without touching any tmux server.
    fn terminal_harness(args: &[&str]) -> Harness {
        let worker = worker("box", "box");
        let session = session("$0", "work");
        let mut harness = Harness::new(
            vec![worker.clone()],
            vec![ready(
                &worker,
                vec![session.clone(), self::session("$1", "other")],
            )],
        );
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let attachment = crate::ssh_mux::begin_attachment();
        let terminal = EmbeddedTerminal::spawn(&args, 24, 80).unwrap();
        harness.picker.active = Some(ActiveTerminal {
            worker,
            session: Some(session),
            started: Instant::now(),
            terminal,
            _attachment: attachment,
        });
        harness.picker.terminal_focused = true;
        harness.buffer(120, 30);
        harness
    }

    #[test]
    fn terminal_focus_keeps_shortcuts_and_paste_out_of_the_picker() {
        let mut harness = terminal_harness(&["/bin/cat"]);
        let generation = harness.picker.generation;
        harness.press(&["q", "r", "tab", "/", "1", "2"]);
        assert!(!harness.picker.quit);
        assert!(!harness.picker.filtering);
        assert_eq!(harness.picker.generation, generation);
        assert!(
            harness
                .picker
                .update(Message::Paste("q\nr\t".into()))
                .is_none()
        );
        assert!(!harness.picker.quit);
        assert_eq!(harness.picker.generation, generation);
        assert!(
            harness
                .picker
                .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(!harness.picker.quit);
        assert!(harness.picker.active.is_some());
    }

    #[test]
    fn directional_focus_preserves_client_and_active_identity_while_browsing() {
        let mut harness = terminal_harness(&["/bin/cat"]);
        let back = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(harness.picker.key(back).is_none());
        assert!(!harness.picker.terminal_focused);
        assert!(harness.picker.key(back).is_none());
        assert!(!harness.picker.terminal_focused);
        let terminal_columns = (
            harness.picker.terminal_area.x,
            harness.picker.terminal_area.width,
        );
        harness.press(&["/", "other", "enter"]);
        harness.buffer(120, 30);
        assert_eq!(
            (
                harness.picker.terminal_area.x,
                harness.picker.terminal_area.width
            ),
            terminal_columns,
        );
        harness.press(&["esc"]);
        harness.press(&["down", "tab"]);
        assert!(harness.picker.terminal_focused);
        assert_eq!(harness.picker.selected_session().unwrap().1.id, "$1");
        assert_eq!(
            harness
                .picker
                .active
                .as_ref()
                .unwrap()
                .session
                .as_ref()
                .unwrap()
                .id,
            "$0"
        );
        assert!(harness.launched().is_empty());
        assert!(harness.picker.key(back).is_none());
        harness.press(&["1", "2", "tab"]);
        assert!(harness.picker.terminal_focused);
        assert!(harness.launched().is_empty());
        assert!(harness.picker.key(back).is_none());
        harness.press(&["up", "enter"]);
        assert!(harness.launched().is_empty());
        assert!(harness.picker.terminal_focused);
        assert!(harness.picker.key(back).is_none());
        harness.press(&["down", "enter"]);
        let launched = harness.launched();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].session.as_ref().unwrap().id, "$1");
        assert!(!harness.picker.quit);
    }

    #[test]
    fn embedded_panel_stays_visible_when_focused_at_small_sizes() {
        let mut harness = terminal_harness(&["/bin/cat"]);
        for (width, height) in [(120, 30), (80, 24), (40, 12), (10, 5), (2, 2), (1, 1)] {
            harness.buffer(width, height);
            assert!(harness.picker.terminal_visible, "{width}x{height}");
            assert!(harness.picker.terminal_area.width <= width);
            assert!(harness.picker.terminal_area.height <= height);
            assert!(!harness.picker.preview_visible);
        }
        harness
            .picker
            .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        harness.buffer(40, 12);
        assert!(!harness.picker.terminal_visible);
        assert!(harness.picker.list_height > 0);
        assert!(harness.picker.active.is_some());
        harness.press(&["tab"]);
        harness.buffer(40, 12);
        assert!(harness.picker.terminal_visible);
    }

    #[test]
    fn stacked_layout_keeps_the_live_client_size_while_rows_change() {
        let mut harness = terminal_harness(&["/bin/cat"]);
        harness.buffer(70, 40);
        assert!(harness.picker.terminal_visible);
        let area = harness.picker.terminal_area;
        // A rescan collapses the list to probing computers until results return.
        let Command::Discover { generation, cancel } = harness.picker.rescan() else {
            panic!("expected discovery");
        };
        let result = (harness.picker.services.discover)(&cancel);
        let scan = harness
            .picker
            .update(Message::Discovered { generation, result });
        assert!(matches!(scan, Some(Command::Scan { .. })));
        harness.buffer(70, 40);
        assert_eq!(harness.picker.terminal_area, area);
    }

    #[test]
    fn browsing_previews_other_sessions_without_disconnect() {
        let mut harness = terminal_harness(&[
            "/bin/sh",
            "-c",
            "printf 'CONNECTED_OUTPUT\\r\\n'; read line; printf 'BACKGROUND_OUTPUT\\r\\n'; exec /bin/cat",
        ]);
        let started = harness.picker.active.as_ref().unwrap().started;
        let cancel = Cancel::new();
        harness.picker.preview_inflight = Some((("box".into(), "$0".into()), cancel.clone()));
        let now = Instant::now();
        harness.tick(now);
        assert!(cancel.is_cancelled());
        let back = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        harness.picker.key(back);
        harness.buffer(120, 30);
        assert!(harness.picker.terminal_visible);
        assert!(!harness.picker.preview_visible);

        harness.press(&["down"]);
        harness.buffer(120, 30);
        assert!(!harness.picker.terminal_visible);
        assert!(harness.picker.preview_visible);
        harness.tick(now);
        harness.tick(now + PREVIEW_DEBOUNCE);
        let view = harness.rendered(120, 30);
        assert!(view.contains("output of other on box"), "{view}");

        // The hidden PTY must still be drained, not disconnected or paused.
        harness
            .picker
            .active
            .as_mut()
            .unwrap()
            .terminal
            .paste("\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(harness.picker.poll_terminal().is_none());
            if harness
                .picker
                .active
                .as_ref()
                .unwrap()
                .terminal
                .contents()
                .contains("BACKGROUND_OUTPUT")
            {
                break;
            }
            assert!(Instant::now() < deadline, "hidden PTY stopped draining");
            std::thread::sleep(Duration::from_millis(5));
        }

        harness.press(&["tab"]);
        let view = harness.rendered(120, 30);
        assert!(view.contains("BACKGROUND_OUTPUT"), "{view}");
        assert!(!view.contains("output of other on box"), "{view}");
        assert!(harness.picker.terminal_visible);
        assert!(!harness.picker.preview_visible);
        assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
        assert_eq!(harness.picker.selected_session().unwrap().1.id, "$1");
        assert!(harness.launched().is_empty());

        harness.picker.key(back);
        let view = harness.rendered(120, 30);
        assert!(view.contains("output of other on box"), "{view}");
        harness.press(&["up"]);
        let view = harness.rendered(120, 30);
        assert!(view.contains("BACKGROUND_OUTPUT"), "{view}");
        assert!(harness.picker.terminal_visible);
    }

    #[test]
    fn child_exit_returns_to_list_and_rescans_before_resuming_previews() {
        let mut harness = terminal_harness(&["/bin/sh", "-c", "exit 7"]);
        let generation = harness.picker.generation;
        let deadline = Instant::now() + Duration::from_secs(5);
        let command = loop {
            if let Some(command) = harness.picker.poll_terminal() {
                break command;
            }
            assert!(Instant::now() < deadline, "PTY child did not exit");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(command, Command::Discover { .. }));
        assert!(harness.picker.active.is_none());
        assert!(!harness.picker.terminal_focused);
        assert!(!harness.picker.quit);
        assert_eq!(harness.picker.generation, generation + 1);
        harness.drive(Some(command));
        harness.buffer(120, 30);
        let now = Instant::now();
        harness.tick(now);
        harness.tick(now + PREVIEW_DEBOUNCE);
        assert_eq!(harness.effects.borrow().previews.len(), 1);
    }

    fn sign_in_harness() -> Harness {
        let mut harness = terminal_harness(&["/bin/cat"]);
        let active = harness.picker.active.as_mut().unwrap();
        active.session = None;
        active.worker.destination = Some("sign-in.example".into());
        let worker = active.worker.clone();
        harness.picker.entries[0] = Entry {
            inventory: Some(failed(&worker, Status::AuthRequired, "Permission denied")),
            worker,
        };
        harness.picker.selection = Selection::Computer("box".into());
        harness.picker.rebuild();
        harness
    }

    #[test]
    fn sign_in_refocus_and_cancellation_preserve_the_picker() {
        let mut harness = sign_in_harness();
        let started = harness.picker.active.as_ref().unwrap().started;
        harness
            .picker
            .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        harness.press(&["enter"]);
        assert!(harness.picker.terminal_focused);
        assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
        assert!(harness.launched().is_empty());
        harness
            .picker
            .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(harness.picker.active.is_none());
        assert!(!harness.picker.terminal_focused && !harness.picker.quit);
        assert_eq!(harness.picker.entries[0].status(), Status::AuthRequired);
        assert!(matches!(
            harness.picker.activate(),
            Some(Command::Launch(Choice { session: None, .. }))
        ));
    }

    #[test]
    fn sign_in_timeout_returns_to_a_usable_list() {
        let mut harness = sign_in_harness();
        harness.picker.active.as_mut().unwrap().started = Instant::now() - Duration::from_secs(121);
        assert!(harness.picker.poll_terminal().is_none());
        assert!(harness.picker.active.is_none());
        assert!(!harness.picker.terminal_focused && !harness.picker.quit);
        assert_eq!(harness.picker.entries[0].status(), Status::ProbeFailed);
    }

    #[test]
    fn completed_sign_in_cannot_populate_a_changed_destination() {
        let original = remote("box", "box", "old.example");
        let mut harness = Harness::new(
            vec![original.clone()],
            vec![failed(&original, Status::AuthRequired, "Permission denied")],
        );
        harness.picker.entries[0].worker.destination = Some("new.example".into());
        let next = harness.picker.update(Message::Checked(ready(
            &original,
            vec![session("$1", "old-destination-session")],
        )));
        assert!(matches!(next, Some(Command::Discover { .. })));
        assert!(harness.picker.entries[0].sessions().is_empty());
        assert_eq!(
            harness.picker.entries[0].worker.destination.as_deref(),
            Some("new.example")
        );
    }

    #[test]
    fn pasted_sidebar_text_never_executes_shortcuts_or_control_characters() {
        let mut harness = Harness::new(vec![], vec![]);
        let generation = harness.picker.generation;
        assert!(
            harness
                .picker
                .update(Message::Paste("q\nr\t".into()))
                .is_none()
        );
        assert!(!harness.picker.quit);
        assert_eq!(harness.picker.generation, generation);
        harness.press(&["/"]);
        harness.picker.update(Message::Paste("q\nr\t\u{1b}".into()));
        assert_eq!(harness.picker.filter, "qr");
        assert!(harness.picker.filtering);
    }

    #[test]
    fn filter_selects_exact_computer_and_session() {
        let workers = vec![worker("a", "host-a"), worker("b", "host-b")];
        let inventories = vec![
            ready(&workers[0], vec![session("$0", "work")]),
            ready(
                &workers[1],
                vec![session("$1", "work"), session("$2", "Straße")],
            ),
        ];
        let scenarios: Vec<(&str, Vec<&str>, &str)> = vec![
            ("computer name", vec!["/", "HOST-B", "enter", "enter"], "$1"),
            (
                "case-folded session",
                vec!["/", "strasse", "enter", "enter"],
                "$2",
            ),
            (
                "cleared filter",
                vec!["/", "nope", "esc", "down", "down", "enter"],
                "$2",
            ),
            (
                "edited filter",
                vec!["/", "host-bx", "backspace", "down", "enter", "enter"],
                "$2",
            ),
        ];
        for (name, keys, expected) in scenarios {
            let mut harness = Harness::new(workers.clone(), inventories.clone());
            harness.press(&keys);
            let launched = harness.launched();
            assert_eq!(launched.len(), 1, "{name}");
            assert_eq!(launched[0].worker.key, "b", "{name}");
            assert_eq!(launched[0].session.as_ref().unwrap().id, expected, "{name}");
        }
    }

    #[test]
    fn unified_list_includes_sessions_and_computers_without_sessions() {
        let up = worker("up", "has-work");
        let empty = remote("empty", "no-work", "empty.tail");
        let auth = remote("auth", "needs-key", "auth.tail");
        let mut offline = remote("off", "gone", "off.tail");
        offline.online = Some(false);
        let mut harness = Harness::new(
            vec![up.clone(), empty.clone(), auth.clone(), offline.clone()],
            vec![
                ready(&up, vec![session("$0", "work")]),
                ready(&empty, vec![]),
                failed(&auth, Status::AuthRequired, "Permission denied"),
                failed(&offline, Status::Unreachable, "no route"),
            ],
        );
        let view = harness.rendered(100, 20);
        assert!(view.contains("has-work") && view.contains("work"), "{view}");
        for visible in ["no-work", "needs-key", "gone"] {
            assert!(view.contains(visible), "{visible} missing:\n{view}");
        }
    }

    #[test]
    fn ssh_banners_follow_the_error_they_precede() {
        // OpenSSH 10 prints these before the real failure.
        let detail = "** WARNING: connection is not using a post-quantum key exchange algorithm.\n\
                      ** This session may be vulnerable to \"store now, decrypt later\" attacks.\n\
                      alice@example-host: Permission denied (publickey).";
        assert_eq!(
            first_line(detail),
            "alice@example-host: Permission denied (publickey)."
        );
        assert_eq!(first_line("** only a banner"), "** only a banner");
        let board = remote("example", "example-host", "alice@example-host");
        let mut harness = Harness::new(
            vec![board.clone()],
            vec![failed(&board, Status::AuthRequired, detail)],
        );
        let view = harness.rendered(160, 30);
        let error = view.find("Permission denied").expect("error shown");
        let banner = view.find("post-quantum").expect("banner kept");
        assert!(error < banner, "error first:\n{view}");
    }

    #[test]
    fn unified_list_orders_computers_and_authenticates_inline() {
        let up = worker("up", "has-work");
        let empty = remote("empty", "no-work", "empty.tail");
        let auth = remote("auth", "needs-key", "auth.tail");
        let mut offline = remote("off", "gone", "off.tail");
        offline.online = Some(false);
        let mut harness = Harness::new(
            vec![up.clone(), empty.clone(), auth.clone(), offline.clone()],
            vec![
                ready(&up, vec![session("$0", "work")]),
                ready(&empty, vec![]),
                failed(
                    &auth,
                    Status::AuthRequired,
                    "alice@auth: Permission denied (publickey).",
                ),
                failed(&offline, Status::Unreachable, "no route"),
            ],
        );
        let view = harness.rendered(100, 20);
        let order: Vec<usize> = ["has-work", "needs-key", "no-work", "gone"]
            .iter()
            .map(|name| view.find(name).unwrap_or_else(|| panic!("{name}:\n{view}")))
            .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{view}");
        harness.press(&["down"]);
        let view = harness.rendered(160, 30);
        assert!(
            view.contains("Permission denied") && view.contains("Press u"),
            "{view}"
        );
        assert!(view.contains("NO SIGNAL"), "{view}");
        // Enter connects interactively.
        harness.press(&["enter"]);
        let launched = harness.launched();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].worker.key, "auth");
        assert!(launched[0].session.is_none());
        // Enter on a computer without sessions explains instead of launching.
        harness.press(&["down", "enter"]);
        assert_eq!(harness.launched().len(), 1);
        assert!(
            harness.picker.notice.contains("no tmux sessions"),
            "{}",
            harness.picker.notice
        );
        // Completion expands the computer without stealing selection while browsing.
        harness.picker.update(Message::Checked(ready(
            &auth,
            vec![session("$9", "new-work")],
        )));
        assert_eq!(
            harness.picker.selection,
            Selection::Computer("empty".into())
        );
        harness.press(&["up", "enter"]);
        let launched = harness.launched();
        assert_eq!(launched[1].worker.key, "auth");
        assert_eq!(launched[1].session.as_ref().unwrap().id, "$9");
    }

    #[test]
    fn offline_computers_render_dim_hollow_status_lights() {
        let mut offline = remote("off", "offline-host", "offline.example.net");
        offline.online = Some(false);
        let inventory = failed(&offline, Status::Unreachable, "no route");
        let mut harness = Harness::new(vec![offline], vec![inventory]);
        // Inspect the semantic status style without the selection highlight.
        harness.picker.cursor = None;
        let buffer = harness.buffer(100, 20);
        let light = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "○")
            .expect("offline computer has a hollow status light");
        assert!(light.modifier.contains(Modifier::DIM));
        assert_ne!(Some(light.fg), theme::error().fg);
    }

    #[test]
    fn rescan_expands_selected_computer_and_ignores_stale_results() {
        let worker = remote("dev", "dev", "alice@dev");
        let inventory = failed(
            &worker,
            Status::AuthRequired,
            "alice@dev: Permission denied (publickey).\nsecond diagnostic",
        );
        let mut harness = Harness::new(vec![worker.clone()], vec![inventory]);
        harness.press(&["enter"]);
        let launched = harness.launched();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].worker.key, "dev");
        assert!(launched[0].session.is_none());
        let superseded = harness.picker.generation;
        harness.press(&["r"]);
        let picker = &mut harness.picker;
        assert_ne!(picker.generation, superseded);
        assert!(picker.entries[0].inventory.is_some());
        let ready = ready(&worker, vec![session("$9", "late")]);
        picker.entries[0].inventory = None;
        picker.update(Message::Inventory {
            generation: superseded,
            inventory: Some(ready.clone()),
        });
        assert!(picker.entries[0].inventory.is_none());
        picker.update(Message::Discovered {
            generation: superseded,
            result: Err("stale error".into()),
        });
        assert!(!picker.quit);
        // A current scan result expands the selected computer in place.
        picker.update(Message::Inventory {
            generation: picker.generation,
            inventory: Some(ready.clone()),
        });
        assert_eq!(picker.entries[0].inventory.as_ref(), Some(&ready));
        assert_eq!(
            picker.selection,
            Selection::Session {
                worker: "dev".into(),
                session: "$9".into(),
                pinned: false
            }
        );
        assert!(harness.press(&["q"]));
        assert_eq!(harness.picker.code, 0);
    }

    #[test]
    fn selection_survives_later_results() {
        let workers = vec![worker("a", "first"), worker("b", "second")];
        let inventories = vec![
            ready(&workers[1], vec![session("$1", "kept")]),
            ready(&workers[0], vec![session("$0", "inserted")]),
        ];
        let mut harness = Harness::new(workers, inventories);
        harness.press(&["enter"]);
        let launched = harness.launched();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].worker.key, "b");
        assert_eq!(launched[0].session.as_ref().unwrap().id, "$1");
    }

    #[test]
    fn local_session_requests_attachment_even_inside_another_server() {
        let _env = env_write();
        let _tmux = EnvVar::set("TMUX", "/tmp/another-server,1,0");
        let worker = local_worker();
        let inventory = ready(&worker, vec![session("$0", "work")]);
        let mut harness = Harness::new(vec![worker], vec![inventory]);
        harness.press(&["enter"]);
        let launched = harness.launched();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].session.as_ref().unwrap().id, "$0");
        assert!(!harness.picker.quit);
    }

    #[test]
    fn escapes_remote_control_sequences() {
        let mut worker = worker("x", "name\x1b[2J");
        worker.destination = Some("evil\x1b]0;title\x07host".into());
        let inventory = failed(&worker, Status::AuthRequired, "detail\x1b[5;7m\nmore\x07");
        let mut harness = Harness::new(vec![worker], vec![inventory]);
        for (width, height) in [(180, 30), (90, 30)] {
            let view = harness.rendered(width, height);
            assert!(!view.contains('\x1b'), "{view}");
            assert!(!view.contains('\x07'), "{view}");
            assert!(view.contains(r"name\x1b[2J"), "{view}");
        }
    }

    #[test]
    fn discovery_failure_and_interrupt_set_exit_codes() {
        let mut harness = Harness::new(vec![], vec![]);
        let picker = &mut harness.picker;
        picker.update(Message::Discovered {
            generation: picker.generation,
            result: Err("discovery failed".into()),
        });
        assert!(picker.quit);
        assert_eq!(picker.code, 1);
        assert_eq!(picker.error.as_deref(), Some("discovery failed"));
        let mut harness = Harness::new(vec![], vec![]);
        harness.picker.update(Message::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(harness.picker.quit);
        assert_eq!(harness.picker.code, exit_codes::INTERRUPTED);
    }

    #[test]
    fn every_exit_cancels_picker_work_without_cancelling_the_session() {
        for exit in ["quit", "interrupt", "discovery error", "drop"] {
            let parent = Cancel::new();
            let mut harness = Harness::new(vec![], vec![]);
            harness.picker.cancel = parent.child();
            let _ = harness.picker.rescan();
            let scan = harness.picker.scan_cancel.as_ref().unwrap().clone();
            let preview = harness.picker.cancel.child();
            harness.picker.preview_inflight = Some((("host".into(), "$0".into()), preview.clone()));
            match exit {
                "quit" => {
                    harness.press(&["q"]);
                }
                "interrupt" => {
                    harness.picker.update(Message::Key(KeyEvent::new(
                        KeyCode::Char('c'),
                        KeyModifiers::CONTROL,
                    )));
                }
                "discovery error" => {
                    harness.picker.update(Message::Discovered {
                        generation: harness.picker.generation,
                        result: Err("discovery failed".into()),
                    });
                }
                _ => {}
            }
            drop(harness);
            assert!(scan.is_cancelled(), "{exit}: scan");
            assert!(preview.is_cancelled(), "{exit}: preview");
            assert!(!parent.is_cancelled(), "{exit}: parent session");
        }
    }

    #[test]
    fn attaching_rescans_and_reports_failure_without_creating_sessions() {
        let mut harness = Harness::new(vec![], vec![]);
        let generation = harness.picker.generation;
        let next = harness
            .picker
            .update(Message::Attached(Ok(AttachOutcome::Exited(7))));
        harness.drive(next);
        assert_eq!(harness.picker.generation, generation + 1);
        assert_eq!(
            harness.picker.notice,
            "Attachment ended with status 7; no replacement session was created."
        );
        let next = harness
            .picker
            .update(Message::Attached(Err("failure\x1b".into())));
        harness.drive(next);
        assert_eq!(harness.picker.notice, r"Attach failed: failure\x1b");
    }

    #[test]
    fn preview_waits_for_a_resting_selection_and_tracks_it() {
        let workers = vec![worker("a", "box")];
        let mut older = session("$1", "older");
        older.activity = NOW - 600;
        let mut newer = session("$2", "newer");
        newer.activity = NOW - 30;
        let mut harness = Harness::new(
            workers.clone(),
            vec![ready(&workers[0], vec![older, newer])],
        );
        harness.rendered(180, 30);
        let start = Instant::now();
        harness.tick(start);
        harness.tick(start + Duration::from_millis(100));
        assert!(
            harness.effects.borrow().previews.is_empty(),
            "fetched before resting"
        );
        harness.tick(start + PREVIEW_DEBOUNCE);
        // Most recently active session is listed and selected first.
        assert_eq!(
            harness.effects.borrow().previews,
            vec![("a".into(), "$2".into())]
        );
        let view = harness.rendered(180, 30);
        assert!(view.contains("output of newer on box"), "{view}");
        assert!(view.contains("        indented"), "tabs expand:\n{view}");
        assert!(!view.contains('\x1b'));
        // Fresh previews are not refetched; stale ones are.
        harness.tick(start + PREVIEW_DEBOUNCE + PREVIEW_REFRESH / 2);
        assert_eq!(harness.effects.borrow().previews.len(), 1);
        harness.tick(start + PREVIEW_DEBOUNCE + PREVIEW_REFRESH);
        assert_eq!(harness.effects.borrow().previews.len(), 2);
        // A result for a session that is no longer selected is cached but not shown.
        harness.press(&["down"]);
        let later = start + Duration::from_secs(30);
        harness.tick(later);
        // Changing channel shows static first, then the tuning screen.
        let view = harness.rendered(180, 30);
        assert!(view.contains('▒') || view.contains('▓'), "static:\n{view}");
        harness.tick(later + CHANNEL_STATIC);
        let next = harness.picker.update(Message::Preview {
            generation: harness.picker.generation,
            destination: workers[0].destination.clone(),
            key: ("a".into(), "$2".into()),
            session: harness.picker.entries[0]
                .sessions()
                .iter()
                .find(|s| s.id == "$2")
                .unwrap()
                .clone(),
            result: Ok(crate::tmux::Preview {
                text: "late result for newer".into(),
                activity: NOW,
                output: NOW,
            }),
        });
        harness.drive(next);
        let view = harness.rendered(180, 30);
        assert!(!view.contains("late result"), "{view}");
        assert!(view.contains("TUNING"), "{view}");
        // No preview pane, no fetches.
        harness.rendered(60, 12);
        harness.tick(later + PREVIEW_REFRESH * 2);
        assert_eq!(harness.effects.borrow().previews.len(), 2);
    }

    #[test]
    fn preview_updates_live_ages_only_for_the_same_session_identity() {
        let computer = worker("a", "box");
        let mut original = session("$1", "printing");
        original.activity = NOW - 120;
        original.output = NOW - 70;
        let mut harness = Harness::new(
            vec![computer.clone()],
            vec![ready(&computer, vec![original.clone()])],
        );
        let before = harness.rendered(180, 30);
        assert!(before.contains("printing    1m"), "{before}");
        harness.picker.update(Message::Preview {
            generation: harness.picker.generation,
            destination: computer.destination.clone(),
            key: ("a".into(), "$1".into()),
            session: original.clone(),
            result: Ok(crate::tmux::Preview {
                text: "live output".into(),
                activity: NOW - 10,
                output: NOW,
            }),
        });
        let after = harness.rendered(180, 30);
        assert!(after.contains("printing   now"), "{after}");
        assert!(after.contains("output now · used 10s ago"), "{after}");
        let current = &harness.picker.entries[0].sessions()[0];
        assert_eq!((current.activity, current.output), (NOW - 10, NOW));
        for change in [
            (|s: &mut Session| s.created += 1) as fn(&mut Session),
            |s| s.server_pid += 1,
            |s| s.server_start += 1,
            |s| s.socket_path.push('x'),
        ] {
            let mut stale = original.clone();
            change(&mut stale);
            harness.picker.update(Message::Preview {
                generation: harness.picker.generation,
                destination: computer.destination.clone(),
                key: ("a".into(), "$1".into()),
                session: stale,
                result: Ok(crate::tmux::Preview {
                    text: "wrong lifetime".into(),
                    activity: 1,
                    output: 1,
                }),
            });
            assert_eq!(harness.picker.entries[0].sessions()[0].output, NOW);
            assert!(!harness.rendered(180, 30).contains("wrong lifetime"));
        }
    }

    #[test]
    fn preview_access_failure_offers_sign_in_and_recovers_sessions() {
        for status in [Status::AuthRequired, Status::HostTrust] {
            let computer = remote("a", "box", "alice@box");
            let other = worker("b", "local");
            let selected = session("$1", "project");
            let mut harness = Harness::new(
                vec![computer.clone(), other.clone()],
                vec![
                    ready(&computer, vec![selected.clone()]),
                    ready(&other, vec![session("$2", "unrelated")]),
                ],
            );
            harness.picker.show_session("a", "$1");
            harness.rendered(120, 30);
            let start = Instant::now();
            harness.tick(start);
            harness.tick(start + PREVIEW_DEBOUNCE);
            assert!(harness.rendered(120, 30).contains("output of project"));
            harness.picker.services.preview =
                Arc::new(move |_, _, _| Err((status, "SSH access rejected".into())));
            harness.tick(start + PREVIEW_DEBOUNCE + PREVIEW_REFRESH);
            assert_eq!(harness.picker.entries[0].status(), status);
            assert_eq!(harness.picker.selection, Selection::Computer("a".into()));
            assert_eq!(harness.picker.entries[1].sessions()[0].name, "unrelated");
            let view = harness.rendered(120, 30);
            assert!(
                view.contains(match status {
                    Status::AuthRequired => "Sign in",
                    _ => "Trust key",
                }),
                "{view}"
            );
            assert!(!view.contains("output of project"), "{view}");
            harness.press(&["enter"]);
            assert_eq!(
                harness.launched(),
                vec![Choice {
                    worker: computer.clone(),
                    session: None,
                }]
            );
            harness
                .picker
                .update(Message::Checked(ready(&computer, vec![selected])));
            assert_eq!(
                harness.picker.selection,
                Selection::Session {
                    worker: "a".into(),
                    session: "$1".into(),
                    pinned: false,
                }
            );
            assert_eq!(harness.picker.screen().signal, Signal::Tuning);
        }
    }

    #[test]
    fn preview_access_failure_ignores_stale_results_and_local_permissions() {
        for boundary in ["scan", "destination", "session", "local"] {
            let computer = if boundary == "local" {
                worker("a", "box")
            } else {
                remote("a", "box", "alice@box")
            };
            let selected = session("$1", "project");
            let mut harness = Harness::new(
                vec![computer.clone()],
                vec![ready(&computer, vec![selected.clone()])],
            );
            harness.rendered(120, 30);
            let start = Instant::now();
            harness.picker.tick(start);
            let Some(Command::Preview { cancel, .. }) =
                harness.picker.tick(start + PREVIEW_DEBOUNCE)
            else {
                panic!("preview not scheduled");
            };
            let generation = harness.picker.generation;
            let mut previous_session = selected.clone();
            match boundary {
                "scan" => {
                    harness.picker.rescan();
                    assert!(cancel.is_cancelled());
                    assert!(harness.picker.preview_inflight.is_none());
                }
                "destination" => {
                    harness.picker.entries[0].worker.destination = Some("bob@box".into());
                }
                "session" => previous_session.server_start += 1,
                _ => {}
            }
            harness.picker.update(Message::Preview {
                generation,
                destination: computer.destination,
                key: ("a".into(), "$1".into()),
                session: previous_session,
                result: Err((Status::AuthRequired, "Permission denied".into())),
            });
            assert_eq!(
                harness.picker.entries[0].status(),
                Status::Ready,
                "{boundary}"
            );
            assert_eq!(
                harness.picker.entries[0].sessions(),
                &[selected],
                "{boundary}"
            );
        }
    }

    #[test]
    fn replacement_session_reusing_a_tmux_id_never_shows_cached_output() {
        let computer = worker("a", "box");
        let mut harness = Harness::new(
            vec![computer.clone()],
            vec![ready(&computer, vec![session("$1", "old")])],
        );
        harness.rendered(180, 30);
        let start = Instant::now();
        harness.tick(start);
        harness.tick(start + PREVIEW_DEBOUNCE);
        assert!(harness.rendered(180, 30).contains("output of old on box"));
        // The tmux server restarted, and its new session reuses the ID.
        let mut replacement = session("$1", "new");
        replacement.server_pid += 1;
        harness.picker.entries[0].inventory = Some(ready(&computer, vec![replacement]));
        harness.picker.rebuild();
        assert_eq!(harness.picker.screen().signal, Signal::Tuning);
        assert!(!harness.rendered(180, 30).contains("output of old"));
        // It is fetched without waiting for the old capture to go stale.
        harness.tick(start + PREVIEW_DEBOUNCE + PREVIEW_REFRESH / 2);
        assert_eq!(harness.effects.borrow().previews.len(), 2);
        assert!(harness.rendered(180, 30).contains("output of new on box"));
    }

    #[test]
    fn preview_refresh_respects_connection_sharing_and_failure_backoff() {
        for local in [false, true] {
            for shared in [false, true] {
                for succeeded in [false, true] {
                    let computer = if local {
                        worker("a", "box")
                    } else {
                        remote("a", "box", "box.example")
                    };
                    let mut harness = Harness::new(
                        vec![computer.clone()],
                        vec![ready(&computer, vec![session("$1", "main")])],
                    );
                    harness.picker.services.sharing_active = Box::new(move |_| shared);
                    if !succeeded {
                        harness.picker.services.preview =
                            Arc::new(|_, _, _| Err((Status::ProbeFailed, "failed".into())));
                    }
                    harness.rendered(180, 30);
                    let start = Instant::now();
                    harness.tick(start);
                    let fetched = start + PREVIEW_DEBOUNCE;
                    harness.tick(fetched);
                    assert_eq!(harness.effects.borrow().previews.len(), 1);
                    let seconds = if succeeded && (local || shared) { 1 } else { 5 };
                    harness.tick(fetched + Duration::from_millis(seconds * 1000 - 1));
                    assert_eq!(harness.effects.borrow().previews.len(), 1);
                    harness.tick(fetched + Duration::from_secs(seconds));
                    assert_eq!(harness.effects.borrow().previews.len(), 2);
                }
            }
        }
    }

    #[test]
    fn moving_away_cancels_an_inflight_preview() {
        let workers = vec![worker("a", "box")];
        let mut harness = Harness::new(
            workers.clone(),
            vec![ready(
                &workers[0],
                vec![session("$1", "one"), session("$2", "two")],
            )],
        );
        harness.rendered(180, 30);
        let start = Instant::now();
        harness.picker.update(Message::Tick(start));
        let Some(Command::Preview { cancel, .. }) = harness
            .picker
            .update(Message::Tick(start + PREVIEW_DEBOUNCE))
        else {
            panic!("expected a preview fetch");
        };
        harness.press(&["down"]);
        harness
            .picker
            .update(Message::Tick(start + PREVIEW_DEBOUNCE * 2));
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn setting_the_ssh_user_saves_a_mapping_and_rescans() {
        let worker = remote("node-1", "example-host", "example-host.example.net");
        let inventory = failed(
            &worker,
            Status::AuthRequired,
            "bob@example-host: Permission denied",
        );
        let mut harness = Harness::new(vec![worker], vec![inventory]);
        let generation = harness.picker.generation;
        harness.press(&["u", "bad user", "enter"]);
        assert!(harness.effects.borrow().mappings.is_empty());
        assert!(!harness.picker.notice.is_empty());
        assert_eq!(harness.picker.generation, generation);
        harness.press(&["u", "alice"]);
        assert!(
            harness
                .rendered(120, 20)
                .contains("SSH user for example-host: alice")
        );
        harness.press(&["enter"]);
        assert_eq!(
            harness.effects.borrow().mappings,
            vec![("node-1".into(), "alice@example-host.example.net".into())]
        );
        assert_eq!(harness.picker.generation, generation + 1);
        harness.press(&["u", "x", "esc"]);
        assert_eq!(harness.effects.borrow().mappings.len(), 1);
        assert!(harness.picker.input.is_none());
    }

    #[test]
    fn ssh_user_edits_override_discovery_mappings_by_stable_key() {
        for (id, shared, mapping) in [
            ("node-1", false, "node-1"),
            ("", false, "box.example.test"),
            ("shared-node", true, "box.example.test"),
        ] {
            let root = crate::test_support::TempDir::new();
            let path = root.path().join("cli.json");
            let path = path.to_str().unwrap();
            save_host_mapping(path, mapping, "old-host").unwrap();
            let raw = serde_json::json!({
                "BackendState": "Running",
                "Peer": {"peer": {
                    "ID": id,
                    "DNSName": "box.example.test.",
                    "HostName": "box",
                    "OS": "linux",
                    "ShareeNode": shared,
                }},
            })
            .to_string();
            let worker = crate::workers::peer_workers(&raw, &load_config(path).unwrap())
                .unwrap()
                .remove(0);
            let key = worker.key.clone();
            let inventory = failed(&worker, Status::AuthRequired, "Permission denied");
            let mut harness = Harness::new(vec![worker], vec![inventory]);
            let saved_path = path.to_owned();
            harness.picker.services.save_mapping =
                Box::new(move |key, destination| save_host_mapping(&saved_path, key, destination));
            harness.press(&["u", "alice", "enter"]);

            let config = load_config(path).unwrap();
            assert_eq!(config.hosts[&key], "alice@old-host");
            assert!(!config.hosts.contains_key("box"));
            let discovered = crate::workers::peer_workers(&raw, &config).unwrap();
            assert_eq!(discovered.len(), 1);
            assert_eq!(discovered[0].key, key);
            assert_eq!(discovered[0].destination.as_deref(), Some("alice@old-host"));
        }
    }

    #[test]
    fn stale_pickers_merge_operations_and_adopt_saved_state() {
        let root = crate::test_support::TempDir::new();
        let path = root.path().join("state.json");
        let path = path.to_str().unwrap();
        let mut pickers: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|key| {
                let worker = worker(key, key);
                let inventory = ready(&worker, vec![session("$0", "main")]);
                let mut harness =
                    Harness::with_loaded_state(vec![worker], vec![inventory], load_state(path));
                let saved_path = path.to_owned();
                harness.picker.services.update_state =
                    Box::new(move |change| update_state(&saved_path, change));
                harness
            })
            .collect();
        pickers[0].press(&["f", "enter"]);
        pickers[1].press(&["f", "enter"]);
        assert!(pickers[1].picker.state.is_pinned("a", "main"));
        assert!(pickers[1].picker.state.is_pinned("b", "main"));
        pickers[0].press(&["f"]);
        let saved = load_state(path).unwrap();
        assert!(!saved.is_pinned("a", "main"));
        assert!(saved.is_pinned("b", "main"));
        assert_eq!(saved.recent.len(), 2);
        assert_eq!(saved.recent[0].worker, "b");
        assert_eq!(saved.recent[1].worker, "a");
        assert_eq!(pickers[0].picker.state, saved);
    }

    #[test]
    fn pins_and_recents_are_shown_and_saved() {
        let workers = vec![worker("a", "alpha"), worker("b", "beta")];
        let inventories = vec![
            ready(&workers[0], vec![session("$0", "main")]),
            ready(
                &workers[1],
                vec![session("$1", "work"), session("$2", "side")],
            ),
        ];
        let state = State {
            pins: vec![Pin {
                worker: "b".into(),
                session: "work".into(),
            }],
            ..State::default()
        };
        let mut harness = Harness::with_state(workers, inventories, state);
        let view = harness.rendered(180, 30);
        assert!(view.contains("Pinned") && view.contains("★ work"), "{view}");
        // Pinned rows lead the list; one attaches by name to the live session.
        harness.press(&["g", "enter"]);
        let launched = harness.launched();
        assert_eq!(launched[0].worker.key, "b");
        assert_eq!(launched[0].session.as_ref().unwrap().id, "$1");
        let saved = harness.effects.borrow().states.last().cloned().unwrap();
        assert_eq!(saved.recent[0].worker, "b");
        assert_eq!(saved.recent[0].session, "work");
        // Pinning another session saves it; it then appears in the pinned section.
        harness.press(&["G", "f"]);
        let saved = harness.effects.borrow().states.last().cloned().unwrap();
        assert!(saved.is_pinned("b", "side"), "{saved:?}");
        assert!(harness.rendered(180, 30).contains("★ side"));
        // A pin whose session is gone is not offered.
        harness.picker.state.toggle_pin("a", "missing");
        harness.picker.rebuild();
        assert!(!harness.rendered(180, 30).contains("missing"));
    }

    #[test]
    fn launching_lists_the_session_as_recent_and_keeps_it_selected() {
        let computer = worker("a", "box");
        let mut harness = Harness::new(
            vec![computer.clone()],
            vec![ready(
                &computer,
                vec![session("$0", "main"), session("$1", "side")],
            )],
        );
        harness.press(&["down", "enter"]);
        let launched = harness.launched();
        assert_eq!(launched.len(), 1);
        assert_eq!(launched[0].session.as_ref().unwrap().id, "$1");
        let picker = &harness.picker;
        let recents: Vec<&str> = picker
            .rows
            .iter()
            .filter(|row| row.kind == RowKind::Pinned && !row.favorite)
            .filter_map(|row| picker.session(*row))
            .map(|session| session.id.as_str())
            .collect();
        assert_eq!(recents, ["$1"]);
        assert!(
            picker
                .current_row()
                .is_some_and(|row| row.kind == RowKind::Session)
        );
        assert_eq!(picker.selected_session().unwrap().1.id, "$1");
    }

    #[test]
    fn pinned_rows_keep_their_computer_visible_in_the_sidebar() {
        let workers = vec![worker("a", "studio"), worker("b", "laptop")];
        let long = "a-really-long-session-name";
        let inventories = vec![
            ready(&workers[0], vec![session("$0", "main")]),
            ready(
                &workers[1],
                vec![session("$1", "main"), session("$2", long)],
            ),
        ];
        let pin = |worker: &str, session: &str| Pin {
            worker: worker.into(),
            session: session.into(),
        };
        let state = State {
            pins: vec![pin("a", "main"), pin("b", "main"), pin("b", long)],
            ..State::default()
        };
        let mut harness = Harness::with_state(workers, inventories, state);
        let buffer = harness.buffer(100, 20);
        // The sidebar's text inside its borders, one entry per row.
        let sidebar: Vec<String> = (0..20u16)
            .map(|y| {
                (1..31u16)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect();
        let header = sidebar
            .iter()
            .position(|line| line.contains("Pinned"))
            .expect("pinned section");
        let pinned = &sidebar[header + 1..header + 4];
        for (line, host) in pinned.iter().zip(["studio", "laptop", "laptop"]) {
            assert!(line.ends_with(host), "{host}: {sidebar:#?}");
        }
        assert!(pinned[2].contains("a-really-lo") && pinned[2].contains('…'));
    }

    #[test]
    fn state_save_failures_are_reported() {
        let workers = vec![worker("a", "alpha")];
        let mut harness = Harness::new(
            workers.clone(),
            vec![ready(&workers[0], vec![session("$0", "main")])],
        );
        harness.picker.services.update_state = Box::new(|_| Err("disk full".into()));
        harness.press(&["f"]);
        assert!(harness.rendered(180, 30).contains("disk full"));
    }

    #[test]
    fn unreadable_state_is_reported_and_never_overwritten_by_picker_actions() {
        let root = crate::test_support::TempDir::new();
        let path = root.path().join("state.json");
        let raw = r#"{"favorites":[{"computer":"old","session":"saved"}]}"#;
        std::fs::write(&path, raw).unwrap();
        let path = path.to_str().unwrap();
        let loaded = load_state(path);
        let error = loaded.as_ref().unwrap_err();
        assert!(error.starts_with(path));
        assert!(error.contains("move it aside to reset pins"));
        let computer = worker("a", "box");
        let mut harness = Harness::with_loaded_state(
            vec![computer.clone()],
            vec![ready(&computer, vec![session("$1", "main")])],
            loaded,
        );
        let saved_path = path.to_owned();
        harness.picker.services.update_state =
            Box::new(move |change| update_state(&saved_path, change));
        harness.press(&["f", "enter"]);
        assert!(harness.picker.state.is_pinned("a", "main"));
        assert_eq!(harness.picker.state.recent.len(), 1);
        assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
        assert!(
            harness
                .rendered(240, 30)
                .contains("unreadable picker state")
        );
        assert!(!harness.picker.state_writable);
    }

    fn wheel(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn wheel_coordinates_follow_rendered_terminal_content() {
        let mut harness = terminal_harness(&["/bin/cat"]);
        for (width, height) in [(120, 30), (70, 40), (40, 12), (2, 2), (1, 1)] {
            harness.buffer(width, height);
            let area = harness.picker.terminal_area;
            for kind in [
                MouseEventKind::ScrollUp,
                MouseEventKind::ScrollDown,
                MouseEventKind::ScrollLeft,
                MouseEventKind::ScrollRight,
            ] {
                for (column, row) in [(0, 0), (area.width - 1, area.height - 1)] {
                    let mut event = wheel(kind, area.x + column, area.y + row);
                    event.modifiers = KeyModifiers::SHIFT | KeyModifiers::CONTROL;
                    assert_eq!(
                        harness.picker.terminal_mouse_event(event),
                        Some(MouseEvent {
                            column,
                            row,
                            ..event
                        }),
                        "{width}x{height}",
                    );
                }
            }
        }
    }

    #[test]
    fn focused_terminal_wheel_reaches_the_child_at_the_translated_cell() {
        let mut harness = terminal_harness(&[
            "/bin/sh",
            "-c",
            "stty raw -echo; printf '\\033[?1000h\\033[?1006hWHEEL_READY'; \
             dd bs=1 count=10 2>/dev/null | od -An -tx1; sleep 30",
        ]);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(harness.picker.poll_terminal().is_none());
            if harness.rendered(120, 30).contains("WHEEL_READY") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "PTY mouse reporting did not start"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let area = harness.picker.terminal_area;
        assert!(
            harness
                .picker
                .update(Message::Mouse(wheel(
                    MouseEventKind::ScrollDown,
                    area.x + 2,
                    area.y + 3,
                )))
                .is_none()
        );
        loop {
            assert!(harness.picker.poll_terminal().is_none());
            if harness
                .rendered(120, 30)
                .split_whitespace()
                .collect::<String>()
                .contains("1b5b3c36353b333b344d")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "child did not receive the SGR wheel"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(harness.picker.notice.is_empty());
    }

    #[test]
    fn wheel_ignores_borders_other_panes_and_nonwheel_events() {
        let mut harness = terminal_harness(&["/bin/cat"]);
        let area = harness.picker.terminal_area;
        let list = harness.picker.list_area;
        let selection = harness.picker.selection.clone();
        for (column, row) in [
            (area.x - 1, area.y),
            (area.x, area.y - 1),
            (area.right(), area.y),
            (area.x, area.bottom()),
            (list.x, list.y),
            (0, 0),
        ] {
            let event = wheel(MouseEventKind::ScrollDown, column, row);
            assert!(harness.picker.terminal_mouse_event(event).is_none());
            assert!(harness.picker.update(Message::Mouse(event)).is_none());
        }
        for kind in [
            MouseEventKind::Down(event::MouseButton::Left),
            MouseEventKind::Up(event::MouseButton::Left),
            MouseEventKind::Drag(event::MouseButton::Left),
            MouseEventKind::Moved,
        ] {
            let event = wheel(kind, area.x, area.y);
            assert!(harness.picker.terminal_mouse_event(event).is_none());
            assert!(harness.picker.update(Message::Mouse(event)).is_none());
        }
        assert_eq!(harness.picker.selection, selection);
        assert!(harness.picker.notice.is_empty());
        assert!(harness.picker.terminal_focused);
    }

    #[test]
    fn sidebar_wheel_uses_rendered_bounds_and_explicit_focus() {
        let mut harness = terminal_harness(&["/bin/cat"]);
        for (width, height) in [(120, 30), (70, 40), (40, 12)] {
            harness
                .picker
                .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
            harness.buffer(width, height);
            let list = harness.picker.list_area;
            let initial = harness.picker.selection.clone();
            let down = wheel(MouseEventKind::ScrollDown, list.right() - 1, list.y);
            assert!(harness.picker.update(Message::Mouse(down)).is_none());
            assert_ne!(harness.picker.selection, initial);
            let up = MouseEvent {
                kind: MouseEventKind::ScrollUp,
                ..down
            };
            assert!(harness.picker.update(Message::Mouse(up)).is_none());
            assert_eq!(harness.picker.selection, initial);
            for event in [
                wheel(MouseEventKind::ScrollDown, list.x - 1, list.y),
                wheel(MouseEventKind::ScrollDown, list.x, list.y - 1),
                wheel(MouseEventKind::ScrollRight, list.x, list.y),
            ] {
                assert!(harness.picker.update(Message::Mouse(event)).is_none());
                assert_eq!(harness.picker.selection, initial);
            }
            if harness.picker.terminal_visible {
                let area = harness.picker.terminal_area;
                let event = wheel(MouseEventKind::ScrollDown, area.x, area.y);
                assert!(harness.picker.terminal_mouse_event(event).is_none());
                assert!(harness.picker.update(Message::Mouse(event)).is_none());
                assert_eq!(harness.picker.selection, initial);
                assert!(harness.picker.notice.is_empty());
            }
            harness.press(&["tab"]);
            assert!(harness.picker.update(Message::Mouse(down)).is_none());
            assert_eq!(harness.picker.selection, initial);
        }
    }

    #[test]
    fn wheel_requires_visible_active_terminal_without_dialogs() {
        let mut harness = terminal_harness(&["/bin/cat"]);
        let area = harness.picker.terminal_area;
        let event = wheel(MouseEventKind::ScrollDown, area.x, area.y);
        harness.picker.filtering = true;
        assert!(harness.picker.terminal_mouse_event(event).is_none());
        assert!(harness.picker.update(Message::Mouse(event)).is_none());
        harness.picker.filtering = false;
        harness.picker.input = Some(UserInput {
            key: "box".into(),
            name: "box".into(),
            destination: "box".into(),
            text: String::new(),
        });
        assert!(harness.picker.terminal_mouse_event(event).is_none());
        assert!(harness.picker.update(Message::Mouse(event)).is_none());
        harness.picker.input = None;
        harness
            .picker
            .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        harness.buffer(40, 12);
        harness.press(&["tab"]);
        assert!(!harness.picker.terminal_visible);
        assert!(harness.picker.terminal_mouse_event(event).is_none());
        assert!(harness.picker.update(Message::Mouse(event)).is_none());
        assert!(harness.picker.notice.is_empty());
        harness.buffer(40, 12);
        assert_eq!(harness.picker.list_area, Rect::default());
        let area = harness.picker.terminal_area;
        let event = wheel(MouseEventKind::ScrollDown, area.x, area.y);
        assert!(harness.picker.terminal_mouse_event(event).is_some());
        assert!(harness.picker.update(Message::Mouse(event)).is_none());
        assert!(harness.picker.notice.contains("tmux prefix+["));
        assert!(harness.picker.notice.contains("set -g mouse on"));
        harness.picker.notice.clear();
        harness.picker.active = None;
        assert!(harness.picker.terminal_mouse_event(event).is_none());
        assert!(harness.picker.update(Message::Mouse(event)).is_none());
        assert!(harness.picker.notice.is_empty());
    }
}
