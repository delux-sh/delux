//! Terminal lifetime, background commands, and the picker event loop.

use std::io::{self, Stdout};
use std::panic::{self, PanicHookInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::Terminal;
use ratatui::crossterm::{
    cursor::{Hide, Show},
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    style::Print,
    terminal::{
        Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};

use super::{ActiveTerminal, Command, Message, Picker, Selection, Services};
use crate::app::{
    AttachOutcome, Inventory, Status, attach, computer_command, failure, preview, scan,
    switching_local_session,
};
use crate::cancel::Cancel;
use crate::display::display;
use crate::embedded::EmbeddedTerminal;
use crate::exit_codes;
use crate::options::Options;
use crate::state::{default_state_path, load_state, update_state};
use crate::tmux::{INVENTORY_SCRIPT, attach_script, parse_inventory};
use crate::tui_backend::TuiBackend;
use crate::workers::{Config, discover, load_config, save_host_mapping};

// Request wheel/button reports, not the unimplemented all-motion tracking mode.
const ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1006h";
const DISABLE_MOUSE: &str = "\x1b[?1000l\x1b[?1006l";

impl Picker {
    pub(super) fn poll_terminal(&mut self) -> Option<Command> {
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

    pub(super) fn finish_terminal(&mut self, result: Result<i32, String>) -> Option<Command> {
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
fn fresh_terminal() -> io::Result<Terminal<TuiBackend<Stdout>>> {
    execute!(io::stdout(), Clear(ClearType::All))?;
    Terminal::new(TuiBackend::new(io::stdout()))
}

fn execute_command(
    command: Command,
    picker: &mut Picker,
    sender: &mpsc::Sender<Message>,
    terminal: &mut Terminal<TuiBackend<Stdout>>,
    guard: &TerminalGuard,
) -> Result<(), String> {
    match command {
        Command::ReportingSetup {
            epoch,
            cancel,
            workers,
            token,
        } => {
            let reporting = picker.services.reporting_setup.clone();
            let sender = sender.clone();
            std::thread::spawn(move || {
                for worker in workers {
                    if cancel.is_cancelled() {
                        break;
                    }
                    let result = reporting(&cancel, &worker, token.as_deref());
                    if cancel.is_cancelled()
                        || sender
                            .send(Message::ReportingSetup {
                                epoch,
                                worker,
                                result,
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            });
        }
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
        Command::AgentStatuses {
            generation,
            cancel,
            workers,
        } => {
            let refresh = picker.services.agent_status.clone();
            let sender = sender.clone();
            std::thread::spawn(move || {
                let mut results = Vec::with_capacity(workers.len());
                for worker in workers {
                    if cancel.is_cancelled() {
                        break;
                    }
                    let result = refresh(&cancel, &worker);
                    results.push((worker, result));
                }
                let _ = sender.send(Message::AgentStatuses {
                    generation,
                    results,
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
        Command::ResolveUser { worker, cancel } => {
            let resolve = picker.services.resolve_user.clone();
            let sender = sender.clone();
            std::thread::spawn(move || {
                let result = resolve(
                    &cancel,
                    worker
                        .destination
                        .as_deref()
                        .expect("remote username lookup"),
                );
                if !cancel.is_cancelled() {
                    let _ = sender.send(Message::SshUser {
                        worker,
                        cancel,
                        result,
                    });
                }
            });
        }
        Command::CompleteDirectory {
            epoch,
            worker,
            path,
            cancel,
        } => {
            let sender = sender.clone();
            std::thread::spawn(move || {
                let result = crate::app::complete_directory(&cancel, &worker, &path);
                if !cancel.is_cancelled() {
                    let _ = sender.send(Message::DirectoryCompleted {
                        epoch,
                        worker,
                        path,
                        result,
                    });
                }
            });
        }
        Command::Create {
            worker,
            request,
            cancel,
        } => {
            let sender = sender.clone();
            std::thread::spawn(move || {
                let result = crate::app::create_session(&cancel, &worker, &request);
                // Even cancelled requests must settle: their remote mutation
                // may have completed before SSH was interrupted.
                let _ = sender.send(Message::Created { worker, result });
            });
        }
        Command::End {
            worker,
            session,
            cancel,
        } => {
            let sender = sender.clone();
            std::thread::spawn(move || {
                let result = crate::app::end_session(&cancel, &worker, &session);
                // Cancellation cannot undo a mutation already accepted remotely.
                let _ = sender.send(Message::Ended {
                    worker,
                    session,
                    result,
                });
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
                Some(session) => attach_script(session, true)
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
                    if signing_in && picker.notice.is_empty() {
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
    let add_config_path = config_path.clone();
    let state_path = default_state_path();
    let state = state_path.clone().and_then(|path| load_state(&path));
    let services = Services {
        discover: Arc::new(move |cancel| {
            let config: Config = load_config(&discover_opts.config_path)?;
            discover(cancel, &config, &discover_opts)
        }),
        scan: Arc::new(scan),
        preview: Arc::new(preview),
        agent_status: Arc::new(crate::app::agent_status),
        reporting_setup: Arc::new(crate::app::reporting_setup),
        resolve_user: Arc::new(crate::app::resolve_ssh_user),
        sharing_active: Box::new(crate::ssh_mux::sharing_active),
        save_mapping: Box::new(move |name, destination| {
            save_host_mapping(&config_path, name, destination)
        }),
        add_mapping: Box::new(move |name, destination| {
            crate::workers::add_host_mapping(&add_config_path, name, destination)
        }),
        add_computer_unavailable: super::add_computer_unavailable(opts),
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
    let initial = picker.rescan(true);
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
        // Tick first so a selection change shows static on the very next frame.
        if let Some(command) = picker.update(Message::Tick(Instant::now())) {
            execute_command(command, &mut picker, &sender, &mut terminal, &guard)?;
        }
        terminal
            .draw(|frame| picker.render(frame))
            .map_err(|error| error.to_string())?;
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
