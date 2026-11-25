use std::ffi::OsString;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use unicode_width::UnicodeWidthStr;

use crate::cancel::Cancel;
use crate::display::display;
use crate::exit_codes::{
    INTERRUPTED, MISSING_TMUX, MISSING_TOOLS, PROBE_FAILED, SESSION_CHANGED, UNSUPPORTED_TMUX,
};
use crate::options::{Options, parse_options, print_help};
use crate::process::{CommandError, CommandResult, run_command};
use crate::terminal::{is_terminal, restore_terminal, terminal_state};
use crate::tmux::{
    INVENTORY_SCRIPT, NewSession, Session, attach_script, create_script, parse_created_session,
    parse_inventory, parse_preview, preview_script, shell_quote, switch_script,
};
use crate::workers::{Config, Worker, discover, explicit_worker, load_config};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Ready,
    #[serde(rename = "probing")]
    Scanning,
    MissingTmux,
    MissingTools,
    ProbeFailed,
    HostTrust,
    AuthRequired,
    Unreachable,
}

impl std::fmt::Display for Status {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Ready => "ready",
            Self::Scanning => "probing",
            Self::MissingTmux => "missing-tmux",
            Self::MissingTools => "missing-tools",
            Self::ProbeFailed => "probe-failed",
            Self::HostTrust => "host-trust",
            Self::AuthRequired => "auth-required",
            Self::Unreachable => "unreachable",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Inventory {
    #[serde(rename = "computer")]
    pub worker: Worker,
    pub sessions: Vec<Session>,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub worker: Worker,
    pub session: Option<Session>,
}

pub fn ssh_command(worker: &Worker, script: &str, interactive: bool, attach: bool) -> Vec<String> {
    let tty = if attach { "-tt" } else { "-T" };
    let trust = if interactive {
        "StrictHostKeyChecking=ask"
    } else {
        "StrictHostKeyChecking=yes"
    };
    let mut args = vec!["ssh".to_owned()];
    crate::lan::add_options(
        &mut args,
        worker
            .destination
            .as_deref()
            .expect("ssh_command requires a remote computer"),
    );
    args.extend(
        [
            tty,
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ConnectionAttempts=1",
            "-o",
            trust,
        ]
        .into_iter()
        .map(str::to_owned),
    );
    if !interactive {
        args.extend(["-o", "BatchMode=yes"].into_iter().map(str::to_owned));
    }
    crate::ssh_mux::add_options(
        &mut args,
        worker
            .destination
            .as_deref()
            .expect("ssh_command requires a remote computer"),
    );
    args.extend(
        [
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ForwardAgent=no",
            "-o",
            "ForwardX11=no",
        ]
        .into_iter()
        .map(str::to_owned),
    );
    args.push("--".to_owned());
    args.push(
        worker
            .destination
            .as_ref()
            .expect("ssh_command requires a remote computer")
            .clone(),
    );
    args.push(format!("/bin/sh -c {}", shell_quote(script)));
    args
}
pub(crate) fn computer_command(
    worker: &Worker,
    script: &str,
    interactive: bool,
    attach: bool,
) -> Vec<String> {
    if worker.destination.is_some() {
        ssh_command(worker, script, interactive, attach)
    } else {
        vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()]
    }
}

/// Ask OpenSSH to resolve aliases, Includes and Match rules without connecting.
pub(crate) fn resolve_ssh_user(cancel: &Cancel, destination: &str) -> Result<String, String> {
    crate::workers::validate_destination(destination)?;
    let args = ["ssh", "-G", "--", destination].map(str::to_owned);
    let result = run_command(cancel, &args, Duration::from_secs(5), false)
        .map_err(|error| error.to_string())?;
    if result.code != 0 {
        return Err(format!(
            "SSH settings lookup exited {}: {}",
            result.code,
            display(result.stderr.trim(), 300),
        ));
    }
    result
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("user "))
        .filter(|user| !user.is_empty() && !user.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or_else(|| "SSH settings did not provide a username.".into())
}

fn run_computer_command(
    cancel: &Cancel,
    worker: &Worker,
    script: &str,
    timeout: Duration,
    interactive: bool,
) -> Result<CommandResult, CommandError> {
    let command = computer_command(worker, script, interactive, false);
    let result = run_command(cancel, &command, timeout, interactive);
    if (matches!(result, Err(CommandError::Timeout(_)))
        || result.as_ref().is_ok_and(|result| result.code == 255))
        && command.iter().any(|arg| arg == "ControlMaster=auto")
        && let Some(destination) = worker.destination.as_deref()
    {
        crate::ssh_mux::reset_destination(destination);
    }
    result
}

pub fn failure(code: i32, stderr: &str) -> (Status, String) {
    if code == UNSUPPORTED_TMUX {
        return (
            Status::ProbeFailed,
            if stderr.trim().is_empty() {
                "Delux requires tmux 3.2 or newer with -N support; upgrade tmux on this computer and retry."
                    .to_owned()
            } else {
                stderr.trim().to_owned()
            },
        );
    }
    let fixed = match code {
        MISSING_TMUX => Some((
            Status::MissingTmux,
            "tmux is not on this computer's non-interactive PATH",
        )),
        MISSING_TOOLS => Some((
            Status::MissingTools,
            "The computer needs working base64 and tr commands",
        )),
        PROBE_FAILED => Some((
            Status::ProbeFailed,
            "tmux inventory changed or could not be read; refresh to retry",
        )),
        _ => None,
    };
    if let Some((status, detail)) = fixed {
        return (status, detail.to_owned());
    }
    let lower = stderr.to_lowercase();
    let detail = stderr.trim();
    let status = if lower.contains("host key verification failed")
        || lower.contains("host identification has changed")
    {
        Status::HostTrust
    } else if lower.contains("permission denied") || lower.contains("authentication failed") {
        Status::AuthRequired
    } else if [
        "connection timed out",
        "connection refused",
        "no route to host",
        "could not resolve hostname",
        "network is unreachable",
    ]
    .iter()
    .any(|message| lower.contains(message))
    {
        Status::Unreachable
    } else {
        Status::ProbeFailed
    };
    let detail = if detail.is_empty() {
        format!("Inventory command exited with status {code}")
    } else {
        detail.to_owned()
    };
    (status, detail)
}

pub fn probe(cancel: &Cancel, worker: &Worker, interactive: bool) -> Inventory {
    if let Some(destination) = &worker.destination {
        crate::lan::prepare(cancel, destination, interactive);
    }
    let mut inventory = Inventory {
        worker: worker.clone(),
        sessions: Vec::new(),
        status: Status::Ready,
        detail: String::new(),
    };
    let timeout = Duration::from_secs(if interactive { 120 } else { 12 });
    match run_computer_command(cancel, worker, INVENTORY_SCRIPT, timeout, interactive) {
        Err(err) => {
            inventory.status = Status::ProbeFailed;
            inventory.detail = err.to_string();
        }
        Ok(result) if result.code != 0 => {
            (inventory.status, inventory.detail) = failure(result.code, &result.stderr);
        }
        Ok(result) => match parse_inventory(&result.stdout) {
            Ok(sessions) => inventory.sessions = sessions,
            Err(err) => {
                inventory.status = Status::ProbeFailed;
                inventory.detail = err;
            }
        },
    }
    inventory
}

/// Create exactly once, without authentication prompts or retries after uncertainty.
/// The returned inventory is from the same verification as the captured identity.
pub fn create_session(
    cancel: &Cancel,
    worker: &Worker,
    request: &NewSession,
) -> Result<(Session, Vec<Session>), (Status, String)> {
    let script = create_script(request).map_err(|error| (Status::ProbeFailed, error))?;
    let uncertain = |(status, detail): (Status, String)| {
        (
            status,
            format!(
                "{}. Rescan before retrying: the session may have been created.",
                detail.trim_end_matches('.')
            ),
        )
    };
    let result = run_computer_command(cancel, worker, &script, Duration::from_secs(12), false)
        .map_err(|error| uncertain((Status::ProbeFailed, error.to_string())))?;
    if result.code != 0 {
        let error = failure(result.code, &result.stderr);
        return Err(match result.code {
            MISSING_TMUX | UNSUPPORTED_TMUX | MISSING_TOOLS => error,
            _ => uncertain(error),
        });
    }
    parse_created_session(&result.stdout).map_err(|error| uncertain((Status::ProbeFailed, error)))
}

/// End exactly once, using the same bounded noninteractive path as creation.
pub fn end_session(
    cancel: &Cancel,
    worker: &Worker,
    session: &Session,
) -> Result<(), (Status, String)> {
    let script = crate::tmux::end_script(session).map_err(|error| (Status::ProbeFailed, error))?;
    let uncertain = |(status, detail): (Status, String)| {
        (
            status,
            format!(
                "{}. The session may already have ended; rescan before trying again.",
                detail.trim_end_matches('.')
            ),
        )
    };
    let result = run_computer_command(cancel, worker, &script, Duration::from_secs(12), false)
        .map_err(|error| uncertain((Status::ProbeFailed, error.to_string())))?;
    let receipt = crate::tmux::parse_ended_session(session, &result.stdout);
    if receipt.is_ok() {
        return Ok(());
    }
    if result.code != 0 {
        let error = failure(result.code, &result.stderr);
        return Err(match result.code {
            MISSING_TMUX | UNSUPPORTED_TMUX | MISSING_TOOLS | SESSION_CHANGED => error,
            _ => uncertain(error),
        });
    }
    receipt.map_err(|error| uncertain((Status::ProbeFailed, error)))
}

/// Capture a preview within five seconds, plus bounded SSH master reset on timeout.
pub fn preview(
    cancel: &Cancel,
    worker: &Worker,
    session: &Session,
) -> Result<crate::tmux::Preview, (Status, String)> {
    let script = preview_script(session).map_err(|error| (Status::ProbeFailed, error))?;
    let result = run_computer_command(cancel, worker, &script, Duration::from_secs(5), false)
        .map_err(|err| (Status::ProbeFailed, err.to_string()))?;
    match result.code {
        0 => parse_preview(&result.stdout).map_err(|error| (Status::ProbeFailed, error)),
        SESSION_CHANGED => Err((
            Status::ProbeFailed,
            "Session changed; press r to rescan".to_owned(),
        )),
        code => Err(failure(code, &result.stderr)),
    }
}

/// Refresh agent status without prompting, within the preview command timeout.
pub fn agent_status(
    cancel: &Cancel,
    worker: &Worker,
) -> Result<Vec<crate::tmux::AgentStatusRow>, String> {
    let result = run_computer_command(
        cancel,
        worker,
        crate::tmux::STATUS_SCRIPT,
        Duration::from_secs(5),
        false,
    )
    .map_err(|err| err.to_string())?;
    if result.code != 0 {
        return Err(failure(result.code, &result.stderr).1);
    }
    crate::tmux::parse_status(&result.stdout)
}

/// Optional reporting setup uses the same bounded, noninteractive transport as
/// inventory. Never retry: an interrupted installation may already have written.
pub(crate) fn reporting_setup(
    cancel: &Cancel,
    worker: &Worker,
    token: Option<&str>,
) -> Result<crate::reporting::Response, String> {
    let result = run_computer_command(
        cancel,
        worker,
        &crate::reporting::command(token),
        Duration::from_secs(20),
        false,
    )
    .map_err(|error| error.to_string())?;
    if result.code != 0 {
        return Err(failure(result.code, &result.stderr).1);
    }
    crate::reporting::parse(&result.stdout, token.is_some())
}

pub fn scan(cancel: &Cancel, workers: Vec<Worker>) -> mpsc::Receiver<Inventory> {
    let (results_tx, results_rx) = mpsc::channel();
    let (jobs_tx, jobs_rx) = mpsc::channel::<Worker>();
    let jobs_rx = Arc::new(Mutex::new(jobs_rx));
    for _ in 0..workers.len().min(8) {
        let jobs = Arc::clone(&jobs_rx);
        let results = results_tx.clone();
        let cancel = cancel.clone();
        thread::spawn(move || {
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                let worker = match jobs.lock() {
                    Ok(receiver) => receiver.recv(),
                    Err(_) => break,
                };
                let Ok(worker) = worker else { break };
                if cancel.is_cancelled() {
                    break;
                }
                let inventory = probe(&cancel, &worker, false);
                if cancel.is_cancelled() || results.send(inventory).is_err() {
                    break;
                }
            }
        });
    }
    drop(results_tx);
    for worker in workers {
        if cancel.is_cancelled() || jobs_tx.send(worker).is_err() {
            break;
        }
    }
    drop(jobs_tx);
    results_rx
}

pub fn interactive_terminal() -> bool {
    is_terminal(rustix::stdio::stdin()) && is_terminal(rustix::stdio::stdout())
}

fn tmux_socket(value: &str) -> Option<&str> {
    let (socket_and_pid, _) = value.rsplit_once(',')?;
    let (socket, _) = socket_and_pid.rsplit_once(',')?;
    (!socket.is_empty()).then_some(socket)
}

pub fn switching_local_session(worker: &Worker, session: &Session) -> Result<bool, String> {
    if worker.destination.is_some() {
        return Ok(false);
    }
    let Some(tmux) = std::env::var_os("TMUX").filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    let current = tmux.to_str().and_then(tmux_socket);
    let same_server = current.is_some_and(|socket| {
        match (
            std::fs::canonicalize(socket),
            std::fs::canonicalize(&session.socket_path),
        ) {
            (Ok(current), Ok(selected)) => current == selected,
            _ => socket == session.socket_path,
        }
    });
    if !same_server {
        return Err(
            "this session is on a different tmux server than the one Delux is running in; detach first"
                .to_owned(),
        );
    }
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachOutcome {
    Exited(i32),
    Switched,
}

pub fn attach(
    cancel: &Cancel,
    worker: &Worker,
    session: &Session,
) -> Result<AttachOutcome, String> {
    if !interactive_terminal() {
        return Err(
            "attachment needs an interactive terminal; use 'delux list --json' for scripts"
                .to_owned(),
        );
    }
    let switching = switching_local_session(worker, session)?;
    let script = if switching {
        switch_script(session)?
    } else {
        attach_script(session, false)?
    };
    let state = terminal_state(rustix::stdio::stdin())
        .ok_or_else(|| io::Error::last_os_error().to_string())?;
    let _attachment = crate::ssh_mux::begin_attachment();
    let result = (|| {
        let args = computer_command(worker, &script, true, true);
        if !switching {
            let _ = writeln!(
                io::stdout(),
                "Attaching to {} / {} ({}). Detach with your tmux prefix then d; the session stays running.",
                display(&worker.name, 200),
                display(&session.name, 200),
                session.id
            );
            let _ = io::stdout().flush();
        }
        if cancel.is_cancelled() {
            return Ok(INTERRUPTED);
        }
        let spawned = Command::new(&args[0])
            .args(&args[1..])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(_) if cancel.is_cancelled() => return Ok(INTERRUPTED),
            Err(err) => return Err(err.to_string()),
        };
        loop {
            if cancel.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(INTERRUPTED);
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    if cancel.is_cancelled() {
                        return Ok(INTERRUPTED);
                    }
                    return Ok(if status.success() {
                        0
                    } else {
                        status.code().unwrap_or(-1).max(1)
                    });
                }
                Ok(None) => thread::sleep(Duration::from_millis(10)),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err.to_string()),
            }
        }
    })();
    restore_terminal(rustix::stdio::stdin(), &state);
    result.map(|code| {
        if switching && code == 0 {
            AttachOutcome::Switched
        } else {
            AttachOutcome::Exited(code)
        }
    })
}

fn show_inventory(out: &mut dyn Write, inventory: &Inventory, separate: bool) {
    if separate {
        let _ = writeln!(out);
    }
    let worker = &inventory.worker;
    let _ = write!(out, "{}", display(&worker.name, 200));
    let destination = worker.destination.as_deref().unwrap_or(&worker.key);
    if destination != worker.name {
        let _ = write!(out, " — {}", display(destination, 200));
    }
    if worker.online == Some(false) {
        let _ = write!(out, " (Tailscale reports offline)");
    }
    let _ = writeln!(out);
    if inventory.status != Status::Ready {
        let _ = writeln!(
            out,
            "  {}: {}",
            inventory.status,
            display(&inventory.detail, 600)
        );
    } else if inventory.sessions.is_empty() {
        let _ = writeln!(out, "  No tmux sessions");
    }
    let mut widths = [0; 3];
    let rows: Vec<_> = inventory
        .sessions
        .iter()
        .map(|session| {
            let name = display(&session.name, 200);
            let windows = if session.windows == 1 {
                "window"
            } else {
                "windows"
            };
            let count_width = session.windows.max(1).ilog10() as usize + 1;
            let row_widths = [
                name.width(),
                session.id.width(),
                count_width + 1 + windows.len() + 1,
            ];
            for (width, row_width) in widths.iter_mut().zip(row_widths) {
                *width = (*width).max(row_width);
            }
            (name, windows, row_widths)
        })
        .collect();
    for (session, (name, windows, row_widths)) in inventory.sessions.iter().zip(rows) {
        let _ = write!(
            out,
            "  {name}{:name_pad$}  {}{:id_pad$}  {} {windows},{:windows_pad$} ",
            "",
            session.id,
            "",
            session.windows,
            "",
            name_pad = widths[0] - row_widths[0],
            id_pad = widths[1] - row_widths[1],
            windows_pad = widths[2] - row_widths[2],
        );
        let _ = match session.attached {
            0 => write!(out, "detached"),
            1 => write!(out, "1 client attached"),
            count => write!(out, "{count} clients attached"),
        };
        if let Some(agent) = session.agent {
            let state = match agent.state {
                crate::tmux::AgentState::Working => "working",
                crate::tmux::AgentState::Waiting => "waiting",
                crate::tmux::AgentState::Done => "done",
                crate::tmux::AgentState::Error => "error",
            };
            let _ = write!(out, "  {state}");
        }
        let _ = writeln!(out);
    }
}

struct InventoryJsonFormatter;

impl serde_json::ser::Formatter for InventoryJsonFormatter {
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut start = 0;
        for (index, ch) in fragment.char_indices() {
            if !matches!(ch, '\u{7f}'..='\u{9f}' | '\u{2028}' | '\u{2029}') {
                continue;
            }
            writer.write_all(&fragment.as_bytes()[start..index])?;
            write!(writer, "\\u{:04x}", u32::from(ch))?;
            start = index + ch.len_utf8();
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }
}

fn run_list(cancel: &Cancel, opts: &Options, config: &Config) -> Result<i32, String> {
    let (workers, warning) = discover(cancel, config, opts)?;
    let mut code = 0;
    if !warning.is_empty() {
        code = 1;
        if !opts.json {
            let _ = writeln!(io::stderr(), "Discovery: {}", display(&warning, 600));
        }
    }
    let mut inventories = Vec::new();
    for (index, inventory) in scan(cancel, workers).into_iter().enumerate() {
        if inventory.status != Status::Ready {
            code = 1;
        }
        if opts.json {
            inventories.push(inventory);
        } else {
            show_inventory(&mut io::stdout(), &inventory, index != 0);
        }
    }
    if cancel.is_cancelled() {
        return Ok(INTERRUPTED);
    }
    if opts.json {
        #[derive(serde::Serialize)]
        struct ListOutput<'a> {
            discovery_warning: Option<&'a str>,
            computers: &'a [Inventory],
        }
        let output = ListOutput {
            discovery_warning: if warning.is_empty() {
                None
            } else {
                Some(&warning)
            },
            computers: &inventories,
        };
        let mut out = io::stdout().lock();
        let mut serializer =
            serde_json::Serializer::with_formatter(&mut out, InventoryJsonFormatter);
        serde::Serialize::serialize(&output, &mut serializer).map_err(|err| err.to_string())?;
        writeln!(out).map_err(|err| err.to_string())?;
    }
    Ok(code)
}

fn run_attach(cancel: &Cancel, opts: &Options, config: &Config) -> Result<i32, String> {
    let worker = explicit_worker(&opts.worker, config)?;
    let mut inventory = probe(cancel, &worker, false);
    if inventory.status != Status::Ready
        && worker.destination.is_some()
        && interactive_terminal()
        && !cancel.is_cancelled()
    {
        let _ = writeln!(
            io::stdout(),
            "{}: {}; checking access interactively.",
            display(&worker.name, 200),
            inventory.status
        );
        let _ = io::stdout().flush();
        inventory = probe(cancel, &worker, true);
    }
    if cancel.is_cancelled() {
        return Ok(INTERRUPTED);
    }
    if inventory.status != Status::Ready {
        return Err(format!("{}: {}", inventory.status, inventory.detail));
    }
    let selected = select_session(&inventory.sessions, &opts.session)?;
    attach(cancel, &worker, selected).map(|outcome| match outcome {
        AttachOutcome::Exited(code) => code,
        AttachOutcome::Switched => 0,
    })
}

fn select_session<'a>(sessions: &'a [Session], selector: &str) -> Result<&'a Session, String> {
    let mut selected = None;
    let by_id = selector
        .strip_prefix('$')
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()));
    for session in sessions {
        let matches = if by_id {
            session.id == selector
        } else {
            session.name == selector
        };
        if matches {
            if selected.is_some() {
                return Err("multiple sessions match; use an exact $ID".to_owned());
            }
            selected = Some(session);
        }
    }
    selected.ok_or_else(|| {
        "no existing session matches that exact name or $ID; use delux list".to_owned()
    })
}

pub fn run(args: Vec<OsString>) -> i32 {
    // Hooks are best-effort even with malformed/non-UTF-8 arguments. Bypass
    // diagnostics, help output, signal setup and discovery entirely.
    if args.first().is_some_and(|arg| arg == "agent-status")
        && args.get(1).is_some_and(|arg| arg == "hook")
    {
        let args: Vec<_> = args[1..]
            .iter()
            .map(|arg| arg.to_str().unwrap_or(""))
            .collect();
        crate::agents::report(&args);
        return 0;
    }
    let args: Result<Vec<_>, _> = args.into_iter().map(OsString::into_string).collect();
    let args = match args {
        Ok(args) => args,
        Err(_) => {
            let _ = writeln!(io::stderr(), "delux: arguments must be valid UTF-8");
            return 2;
        }
    };
    let flag = Arc::new(AtomicBool::new(false));
    let terminated = Arc::new(AtomicBool::new(false));
    let mut signal_ids = Vec::with_capacity(3);
    for (signal, flag) in [
        (signal_hook::consts::SIGINT, &flag),
        (signal_hook::consts::SIGTERM, &terminated),
        (signal_hook::consts::SIGTERM, &flag),
    ] {
        match signal_hook::flag::register(signal, Arc::clone(flag)) {
            Ok(id) => signal_ids.push(id),
            Err(err) => {
                for id in signal_ids {
                    signal_hook::low_level::unregister(id);
                }
                let _ = writeln!(io::stderr(), "delux: {}", display(&err.to_string(), 800));
                return 1;
            }
        }
    }
    let cancel = Cancel::from_flag(flag);
    let code = run_with_cancel(&cancel, &args);
    for id in signal_ids {
        signal_hook::low_level::unregister(id);
    }
    if terminated.load(std::sync::atomic::Ordering::SeqCst) {
        crate::exit_codes::TERMINATED
    } else {
        code
    }
}

fn run_with_cancel(cancel: &Cancel, args: &[String]) -> i32 {
    let opts = match parse_options(args) {
        Ok(opts) => opts,
        Err(err) => {
            let _ = writeln!(io::stderr(), "delux: {}", display(&err, 800));
            return 2;
        }
    };
    if opts.help {
        print_help(&mut io::stdout(), &opts.command);
        return 0;
    }
    if opts.version {
        // Builds outside a Git checkout, such as from a source archive, have no commit.
        let _ = match option_env!("DELUX_COMMIT") {
            Some(commit) => writeln!(
                io::stdout(),
                "delux {} ({commit})",
                env!("CARGO_PKG_VERSION")
            ),
            None => writeln!(io::stdout(), "delux {}", env!("CARGO_PKG_VERSION")),
        };
        return 0;
    }
    if opts.command == "agent-status" {
        let args: Vec<_> = opts.agent_args.iter().map(String::as_str).collect();
        crate::agents::report(&args);
        return 0;
    }
    if opts.command == "agents" {
        return crate::agents::run(&opts.agent_args, &mut io::stdout(), &mut io::stderr());
    }
    crate::ssh_mux::begin();
    let commands = cancel.child();
    let result = load_config(&opts.config_path).and_then(|config| match opts.command.as_str() {
        "list" => run_list(&commands, &opts, &config),
        "attach" => run_attach(&commands, &opts, &config),
        _ if !interactive_terminal() => Err(
            "the picker needs an interactive terminal; use 'delux list' or 'delux list --json'"
                .to_owned(),
        ),
        _ => crate::tui::run_picker(&commands, &opts),
    });
    commands.cancel_and_wait();
    crate::ssh_mux::close();
    if cancel.is_cancelled() {
        let _ = writeln!(io::stderr(), "\nCancelled.");
        return INTERRUPTED;
    }
    match result {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(io::stderr(), "delux: {}", display(&err, 800));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EnvVar, TempDir, env_write};
    use crate::workers::local_worker;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn session() -> Session {
        Session {
            id: "$7".into(),
            name: "work".into(),
            windows: 1,
            attached: 0,
            server_pid: 1234,
            server_start: 1700000000,
            socket_path: "/tmp/tmux-1000/default".into(),
            created: 1700000001,
            activity: 1700000002,
            output: 1700000003,
            command: "sleep".into(),
            path: "/tmp/work".into(),
            agent: None,
        }
    }

    #[test]
    fn attachment_selects_exact_names_or_numeric_ids() {
        let mut named = session();
        named.name = "$work".into();
        let mut numeric_name = session();
        numeric_name.id = "$8".into();
        numeric_name.name = "$7".into();
        let sessions = [named, numeric_name];
        assert_eq!(select_session(&sessions, "$work").unwrap().id, "$7");
        assert_eq!(select_session(&sessions, "$7").unwrap().id, "$7");
        assert_eq!(select_session(&sessions, "$8").unwrap().name, "$7");
        assert!(select_session(&sessions, "$wor").is_err());
        assert!(select_session(&sessions, "$9").is_err());
        let mut duplicate = sessions[0].clone();
        duplicate.id = "$9".into();
        assert!(select_session(&[sessions[0].clone(), duplicate], "$work").is_err());
    }

    #[test]
    fn tmux_socket_preserves_commas_in_the_socket_path() {
        assert_eq!(
            tmux_socket("/tmp/a,b,c/default,123,0"),
            Some("/tmp/a,b,c/default")
        );
        assert_eq!(tmux_socket("/tmp/default,123,0"), Some("/tmp/default"));
        for invalid in ["", "/tmp/default", "/tmp/default,123", ",123,0"] {
            assert_eq!(tmux_socket(invalid), None);
        }
    }

    #[test]
    fn local_switch_requires_the_same_socket_but_accepts_canonical_aliases() {
        let _env = env_write();
        let root = TempDir::new();
        let socket = root.path().join("socket,with,commas");
        let alias = root.path().join("alias");
        fs::write(&socket, "").unwrap();
        symlink(&socket, &alias).unwrap();
        let _tmux = EnvVar::set("TMUX", format!("{},123,0", alias.display()));
        let mut session = session();
        session.socket_path = socket.to_str().unwrap().to_owned();
        assert_eq!(switching_local_session(&local_worker(), &session), Ok(true));
        session.socket_path = root.path().join("different").to_str().unwrap().to_owned();
        assert!(
            switching_local_session(&local_worker(), &session)
                .unwrap_err()
                .contains("different tmux server")
        );
        let _missing = EnvVar::set("TMUX", format!("{},123,0", session.socket_path));
        assert_eq!(switching_local_session(&local_worker(), &session), Ok(true));
        let _empty = EnvVar::set("TMUX", "");
        assert_eq!(
            switching_local_session(&local_worker(), &session),
            Ok(false)
        );
    }

    #[test]
    fn json_escapes_terminal_controls_without_html_escaping() {
        let controls: String = ('\u{7f}'..='\u{9f}').collect();
        let input = format!("<>&{controls}\u{2028}\u{2029}\n");
        let mut output = Vec::new();
        let mut serializer =
            serde_json::Serializer::with_formatter(&mut output, InventoryJsonFormatter);
        serde::Serialize::serialize(&input, &mut serializer).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("\"<>&\\u007f\\u0080"));
        assert!(output.ends_with("\\u009f\\u2028\\u2029\\n\""));
        assert!(!output.chars().any(|ch| matches!(ch, '\u{7f}'..='\u{9f}')));
        assert_eq!(serde_json::from_str::<String>(&output).unwrap(), input);
    }

    #[test]
    fn inventory_json_uses_computer_and_kebab_case_status() {
        let inventory = Inventory {
            worker: local_worker(),
            sessions: Vec::new(),
            status: Status::MissingTmux,
            detail: String::new(),
        };
        let value = serde_json::to_value(inventory).unwrap();
        assert!(value.get("computer").is_some());
        assert!(value.get("worker").is_none());
        assert_eq!(value["status"], "missing-tmux");
    }

    #[test]
    fn inventory_json_includes_agent_status_shape() {
        let mut inventory = Inventory {
            worker: local_worker(),
            sessions: vec![session(); 5],
            status: Status::Ready,
            detail: String::new(),
        };
        for (session, state) in inventory.sessions.iter_mut().skip(1).zip([
            crate::tmux::AgentState::Working,
            crate::tmux::AgentState::Waiting,
            crate::tmux::AgentState::Done,
            crate::tmux::AgentState::Error,
        ]) {
            session.agent = Some(crate::tmux::AgentStatus {
                state,
                since: Some(1700000004),
            });
        }
        let value = serde_json::to_value(&inventory).unwrap();
        assert!(value["sessions"][0]["agent"].is_null());
        for (index, state) in ["working", "waiting", "done", "error"].iter().enumerate() {
            assert_eq!(
                value["sessions"][index + 1]["agent"],
                serde_json::json!({"state": state, "since": 1700000004}),
            );
        }
        inventory.sessions[1].agent.as_mut().unwrap().since = None;
        let value = serde_json::to_value(&inventory).unwrap();
        assert_eq!(
            value["sessions"][1]["agent"],
            serde_json::json!({"state": "working", "since": null}),
        );
    }

    #[test]
    fn human_inventory_uses_plain_rows_and_correct_counts() {
        let mut inventory = Inventory {
            worker: Worker {
                name: "laptop".into(),
                ..local_worker()
            },
            sessions: vec![session()],
            status: Status::Ready,
            detail: String::new(),
        };
        for (windows, attached, expected) in [
            (1, 0, "1 window, detached"),
            (2, 1, "2 windows, 1 client attached"),
            (3, 2, "3 windows, 2 clients attached"),
        ] {
            inventory.sessions[0].windows = windows;
            inventory.sessions[0].attached = attached;
            let mut output = Vec::new();
            show_inventory(&mut output, &inventory, false);
            let output = String::from_utf8(output).unwrap();
            assert_eq!(output, format!("laptop — local\n  work  $7  {expected}\n"));
        }
        inventory.sessions.clear();
        inventory.status = Status::ProbeFailed;
        inventory.detail = "connection refused".into();
        let mut output = Vec::new();
        show_inventory(&mut output, &inventory, false);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "laptop — local\n  probe-failed: connection refused\n",
        );
    }

    #[test]
    fn human_inventory_shows_agent_error() {
        let mut failed = session();
        failed.agent = Some(crate::tmux::AgentStatus {
            state: crate::tmux::AgentState::Error,
            since: None,
        });
        let inventory = Inventory {
            worker: Worker {
                name: "laptop".into(),
                ..local_worker()
            },
            sessions: vec![failed],
            status: Status::Ready,
            detail: String::new(),
        };
        let mut output = Vec::new();
        show_inventory(&mut output, &inventory, false);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "laptop — local\n  work  $7  1 window, detached  error\n",
        );
    }

    #[test]
    fn human_inventory_aligns_display_columns_and_separates_computers() {
        let mut inventory = Inventory {
            worker: Worker {
                name: "laptop".into(),
                ..local_worker()
            },
            sessions: vec![session(); 3],
            status: Status::Ready,
            detail: String::new(),
        };
        inventory.sessions[0].name = "a b".into();
        inventory.sessions[1].name = "東京".into();
        inventory.sessions[1].id = "$123".into();
        inventory.sessions[1].windows = 12;
        inventory.sessions[1].attached = 2;
        inventory.sessions[2].name = "e\u{301}".into();
        inventory.sessions[2].id = "$9".into();
        inventory.sessions[2].attached = 1;
        for (session, state) in inventory.sessions.iter_mut().zip([
            crate::tmux::AgentState::Working,
            crate::tmux::AgentState::Waiting,
            crate::tmux::AgentState::Done,
        ]) {
            session.agent = Some(crate::tmux::AgentStatus { state, since: None });
        }
        let mut output = Vec::new();
        show_inventory(&mut output, &inventory, false);
        inventory.worker.name = "other computer".into();
        inventory.sessions.clear();
        show_inventory(&mut output, &inventory, true);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            output,
            concat!(
                "laptop — local\n",
                "  a b   $7    1 window,   detached  working\n",
                "  東京  $123  12 windows, 2 clients attached  waiting\n",
                "  e\u{301}     $9    1 window,   1 client attached  done\n",
                "\nother computer — local\n",
                "  No tmux sessions\n",
            )
        );
        let rows: Vec<_> = output.lines().skip(1).take(3).collect();
        for (row, attachment) in rows.iter().zip(["detached", "2 clients", "1 client"]) {
            assert_eq!(row[..row.find('$').unwrap()].width(), 8);
            assert_eq!(row[..row.find(attachment).unwrap()].width(), 26);
        }
    }

    #[test]
    fn failure_mapping_prioritizes_protocol_codes_then_trust_and_authentication() {
        for (code, status) in [
            (MISSING_TMUX, Status::MissingTmux),
            (UNSUPPORTED_TMUX, Status::ProbeFailed),
            (MISSING_TOOLS, Status::MissingTools),
            (PROBE_FAILED, Status::ProbeFailed),
        ] {
            assert_eq!(
                failure(code, "Host key verification failed; permission denied").0,
                status
            );
        }
        for (stderr, status) in [
            (
                "Host key verification failed; Permission denied; Connection refused",
                Status::HostTrust,
            ),
            ("REMOTE HOST IDENTIFICATION HAS CHANGED", Status::HostTrust),
            (
                "Permission denied; Connection refused",
                Status::AuthRequired,
            ),
            ("Authentication failed", Status::AuthRequired),
            ("Connection timed out", Status::Unreachable),
            ("Connection refused", Status::Unreachable),
            ("No route to host", Status::Unreachable),
            ("Could not resolve hostname", Status::Unreachable),
            ("Network is unreachable", Status::Unreachable),
            ("Other error", Status::ProbeFailed),
        ] {
            assert_eq!(
                failure(255, &format!(" \n{stderr}\t")),
                (status, stderr.to_owned())
            );
        }
        assert_eq!(
            failure(-1, " \n\t"),
            (
                Status::ProbeFailed,
                "Inventory command exited with status -1".to_owned()
            )
        );
    }

    #[test]
    fn interactive_login_authenticates_background_and_attachment_until_master_closes() {
        let _env = env_write();
        let sshd = std::path::Path::new("/usr/sbin/sshd");
        if !sshd.exists() {
            eprintln!("skipping: SSH authentication regression requires OpenSSH sshd");
            return;
        }
        let root = TempDir::new();
        for (name, passphrase) in [("host", ""), ("client", "delux-test-passphrase")] {
            let output = Command::new("/usr/bin/ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", passphrase, "-f"])
                .arg(root.path().join(name))
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let forward_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let forward_port = forward_listener.local_addr().unwrap().port();
        let user = Command::new("id").arg("-un").output().unwrap();
        assert!(user.status.success(), "{user:?}");
        let user = String::from_utf8(user.stdout).unwrap();
        let directory = root.path().display();
        let config = root.path().join("sshd_config");
        fs::write(
            &config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {directory}/host\n\
                 PidFile {directory}/sshd.pid\nAuthorizedKeysFile {directory}/client.pub\n\
                 StrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n\
                 UsePAM no\nAllowUsers {}\n",
                user.trim()
            ),
        )
        .unwrap();
        let host_key = fs::read_to_string(root.path().join("host.pub")).unwrap();
        fs::write(
            root.path().join("known_hosts"),
            format!("[127.0.0.1]:{port} {host_key}"),
        )
        .unwrap();
        fs::write(
            root.path().join("ssh_config"),
            format!(
                "Host auth-test\nHostName 127.0.0.1\nPort {port}\nUser {}\n\
                 IdentityFile {directory}/client\nIdentityAgent none\nIdentitiesOnly yes\n\
                 UserKnownHostsFile {directory}/known_hosts\nGlobalKnownHostsFile /dev/null\n\
                 ForwardAgent yes\nForwardX11 yes\nExitOnForwardFailure yes\n\
                 LocalForward 127.0.0.1:{forward_port} 127.0.0.1:{port}\n",
                user.trim()
            ),
        )
        .unwrap();
        let ssh = root.path().join("ssh");
        fs::write(
            &ssh,
            format!("#!/bin/sh\nexec /usr/bin/ssh -F '{directory}/ssh_config' \"$@\"\n"),
        )
        .unwrap();
        let askpass = root.path().join("askpass");
        let prompts = root.path().join("prompts");
        fs::write(
            &askpass,
            format!(
                "#!/bin/sh\nprintf x >> '{directory}/prompts'\nprintf '%s\\n' delux-test-passphrase\n"
            ),
        )
        .unwrap();
        for executable in [&ssh, &askpass] {
            fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let _path = EnvVar::set("PATH", root.path());
        let _runtime = EnvVar::set("XDG_RUNTIME_DIR", root.path());
        let _askpass = EnvVar::set("SSH_ASKPASS", &askpass);
        let _require = EnvVar::set("SSH_ASKPASS_REQUIRE", "force");
        let _display = EnvVar::set("DISPLAY", ":0");
        let _agent = EnvVar::remove("SSH_AUTH_SOCK");
        let log_path = root.path().join("sshd.log");
        drop(listener);
        let child = Command::new(sshd)
            .args(["-D", "-e", "-f"])
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log_path).unwrap())
            .spawn()
            .unwrap();
        struct Server(std::process::Child);
        impl Drop for Server {
            fn drop(&mut self) {
                crate::ssh_mux::close();
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut server = Server(child);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                server.0.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline,
                "{}",
                fs::read_to_string(&log_path).unwrap()
            );
            thread::sleep(Duration::from_millis(10));
        }
        crate::ssh_mux::begin();
        let worker = Worker {
            key: "auth-test".into(),
            name: "auth-test".into(),
            destination: Some("auth-test".into()),
            online: Some(true),
        };
        let run = |interactive| {
            run_computer_command(
                &Cancel::new(),
                &worker,
                "printf authenticated",
                Duration::from_secs(10),
                interactive,
            )
            .unwrap()
        };
        let denied = run(false);
        assert_eq!(failure(denied.code, &denied.stderr).0, Status::AuthRequired);
        assert!(!prompts.exists());
        let authenticated = run(true);
        assert_eq!(authenticated.code, 0, "{authenticated:?}");
        assert_eq!(authenticated.stdout, "authenticated");
        let reused = run(false);
        assert_eq!(reused.code, 0, "{reused:?}");
        assert_eq!(reused.stdout, "authenticated");
        let attachment_guard = crate::ssh_mux::begin_attachment();
        let attachment = computer_command(&worker, "test -t 0 && printf attached", true, true);
        let attached =
            run_command(&Cancel::new(), &attachment, Duration::from_secs(10), true).unwrap();
        assert_eq!(attached.code, 0, "{attached:?}");
        assert_eq!(attached.stdout, "attached");
        let timeout = Duration::from_millis(100);
        assert_eq!(
            run_computer_command(&Cancel::new(), &worker, "exec sleep 1", timeout, false),
            Err(CommandError::Timeout(timeout))
        );
        let returned = run(false);
        assert_eq!(returned.code, 0, "{returned:?}");
        assert_eq!(returned.stdout, "authenticated");
        assert_eq!(fs::read_to_string(&prompts).unwrap(), "x");
        drop(attachment_guard);
        assert_eq!(
            run_computer_command(&Cancel::new(), &worker, "exec sleep 1", timeout, false),
            Err(CommandError::Timeout(timeout))
        );
        let denied = run(false);
        assert_eq!(failure(denied.code, &denied.stderr).0, Status::AuthRequired);
        assert_eq!(fs::read_to_string(prompts).unwrap(), "x");
    }

    #[test]
    fn timeouts_without_multiplexing_do_not_issue_control_commands() {
        let _env = env_write();
        let root = TempDir::new();
        let _runtime = EnvVar::set("XDG_RUNTIME_DIR", root.path());
        let _path = EnvVar::set("PATH", root.path());
        let log = root.path().join("calls");
        let _log = EnvVar::set("MUX_LOG", &log);
        let ssh = root.path().join("ssh");
        fs::write(&ssh, "#!/bin/sh\nprintf called >> \"$MUX_LOG\"\n").unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        crate::ssh_mux::begin();
        let worker = Worker {
            key: "remote:host".into(),
            name: "host".into(),
            destination: Some("user@host".into()),
            online: Some(true),
        };
        // An unsafe private directory disables sharing, even for tracked hosts.
        computer_command(&worker, "true", false, false);
        let base = root.path().join("delux");
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!crate::ssh_mux::sharing_active("user@host"));
        for worker in [&worker, &local_worker()] {
            assert_eq!(
                run_computer_command(&Cancel::new(), worker, "true", Duration::ZERO, false),
                Err(CommandError::Timeout(Duration::ZERO))
            );
        }
        assert!(!log.exists());
        crate::ssh_mux::close();
    }

    #[test]
    fn agent_status_remote_is_noninteractive_and_maps_results() {
        let _env = env_write();
        let root = TempDir::new();
        let ssh = root.path().join("ssh");
        fs::write(
            &ssh,
            r#"#!/bin/sh
printf '%s\n' "$@" > "$DELUX_TEST_ARGS"
[ "$SSH_ASKPASS_REQUIRE" = never ] || exit 99
if IFS= read -r input; then exit 99; fi
printf '%s' "$DELUX_TEST_OUTPUT"
printf '%s' "$DELUX_TEST_ERROR" >&2
exit "$DELUX_TEST_CODE"
"#,
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let arguments = root.path().join("arguments");
        let _path = EnvVar::set("PATH", root.path());
        let _arguments = EnvVar::set("DELUX_TEST_ARGS", &arguments);
        let worker = Worker {
            key: "remote:host".into(),
            name: "host".into(),
            destination: Some("user@host".into()),
            online: Some(true),
        };
        let _code = EnvVar::set("DELUX_TEST_CODE", "0");
        let _error = EnvVar::set("DELUX_TEST_ERROR", "");
        let _output = EnvVar::set(
            "DELUX_TEST_OUTPUT",
            "DELUX_STATUS_2\n1234 1700000000 $7 1700000003 waiting 1700000004\nDELUX_END\n",
        );
        let rows = agent_status(&Cancel::new(), &worker).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].server_pid, 1234);
        assert_eq!(rows[0].server_start, 1700000000);
        assert_eq!(rows[0].session_id, "$7");
        assert_eq!(
            rows[0].agent,
            Some(crate::tmux::AgentStatus {
                state: crate::tmux::AgentState::Waiting,
                since: Some(1700000004),
            }),
        );
        let arguments = fs::read_to_string(arguments).unwrap();
        assert!(arguments.starts_with("-T\n"), "{arguments}");
        for option in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "ClearAllForwardings=yes",
            "ForwardAgent=no",
            "ForwardX11=no",
        ] {
            assert!(arguments.contains(&format!("\n{option}\n")), "{arguments}");
        }
        assert!(
            arguments.contains(&shell_quote(crate::tmux::STATUS_SCRIPT)),
            "{arguments}",
        );
        for (code, stderr) in [
            (MISSING_TMUX, ""),
            (MISSING_TOOLS, ""),
            (PROBE_FAILED, ""),
            (UNSUPPORTED_TMUX, "Upgrade tmux"),
            (255, "Permission denied"),
        ] {
            let _code = EnvVar::set("DELUX_TEST_CODE", code.to_string());
            let _error = EnvVar::set("DELUX_TEST_ERROR", stderr);
            assert_eq!(
                agent_status(&Cancel::new(), &worker).unwrap_err(),
                failure(code, stderr).1,
            );
        }
        let _output = EnvVar::set("DELUX_TEST_OUTPUT", "not a status frame");
        assert!(agent_status(&Cancel::new(), &worker).is_err());
        let _output = EnvVar::set("DELUX_TEST_OUTPUT", "DELUX_STATUS_2\nDELUX_END\n");
        assert!(agent_status(&Cancel::new(), &worker).unwrap().is_empty());
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(agent_status(&cancel, &worker).is_err());
    }

    #[test]
    fn preview_remote_is_noninteractive_and_maps_results() {
        let _env = env_write();
        let root = TempDir::new();
        let ssh = root.path().join("ssh");
        fs::write(
            &ssh,
            r#"#!/bin/sh
printf '%s\n' "$@" > "$DELUX_TEST_ARGS"
[ "$SSH_ASKPASS_REQUIRE" = never ] || exit 99
if IFS= read -r input; then exit 99; fi
printf '%s' "$DELUX_TEST_OUTPUT"
printf '%s' "$DELUX_TEST_ERROR" >&2
exit "$DELUX_TEST_CODE"
"#,
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let arguments = root.path().join("arguments");
        let _path = EnvVar::set("PATH", root.path());
        let _arguments = EnvVar::set("DELUX_TEST_ARGS", &arguments);
        let worker = Worker {
            key: "remote:host".into(),
            name: "host".into(),
            destination: Some("user@host".into()),
            online: Some(true),
        };
        let output = format!(
            "DELUX_PREVIEW_2\n100\n200\n{}\nDELUX_END\n",
            STANDARD.encode(b"marker\n\n"),
        );
        for (code, stderr, expected) in [
            (0, "", Ok("marker".to_owned())),
            (
                SESSION_CHANGED,
                "",
                Err((
                    Status::ProbeFailed,
                    "Session changed; press r to rescan".to_owned(),
                )),
            ),
            (MISSING_TMUX, "", Err(failure(MISSING_TMUX, ""))),
            (MISSING_TOOLS, "", Err(failure(MISSING_TOOLS, ""))),
            (PROBE_FAILED, "", Err(failure(PROBE_FAILED, ""))),
            (
                UNSUPPORTED_TMUX,
                "Delux requires tmux 3.2 or newer; upgrade tmux",
                Err((
                    Status::ProbeFailed,
                    "Delux requires tmux 3.2 or newer; upgrade tmux".to_owned(),
                )),
            ),
            (
                255,
                "Permission denied",
                Err((Status::AuthRequired, "Permission denied".to_owned())),
            ),
        ] {
            let _code = EnvVar::set("DELUX_TEST_CODE", code.to_string());
            let _output = EnvVar::set("DELUX_TEST_OUTPUT", &output);
            let _error = EnvVar::set("DELUX_TEST_ERROR", stderr);
            assert_eq!(
                preview(&Cancel::new(), &worker, &session()).map(|preview| preview.text),
                expected
            );
        }
        let arguments = fs::read_to_string(&arguments).unwrap();
        assert!(arguments.starts_with("-T\n"), "{arguments}");
        for option in [
            "BatchMode=yes",
            "StrictHostKeyChecking=yes",
            "ClearAllForwardings=yes",
            "ForwardAgent=no",
            "ForwardX11=no",
        ] {
            assert!(arguments.contains(&format!("\n{option}\n")), "{arguments}");
        }
        assert!(
            arguments.contains("\n--\nuser@host\n/bin/sh -c "),
            "{arguments}"
        );
        assert!(
            !arguments.contains("StrictHostKeyChecking=ask"),
            "{arguments}"
        );
        let _code = EnvVar::set("DELUX_TEST_CODE", "0");
        let _output = EnvVar::set("DELUX_TEST_OUTPUT", "not a preview frame");
        let _error = EnvVar::set("DELUX_TEST_ERROR", "");
        assert!(preview(&Cancel::new(), &worker, &session()).is_err());
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(preview(&cancel, &worker, &session()).is_err());
    }

    #[test]
    fn preview_local_uses_shell_without_ssh() {
        let _env = env_write();
        let root = TempDir::new();
        for name in ["base64", "tr"] {
            let path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|directory| directory.join(name))
                .find(|path| path.is_file());
            let Some(path) = path else {
                eprintln!("skipping: {name} unavailable");
                return;
            };
            symlink(path, root.path().join(name)).unwrap();
        }
        let _path = EnvVar::set("PATH", root.path());
        assert_eq!(
            preview(&Cancel::new(), &local_worker(), &session()),
            Err(failure(MISSING_TMUX, "")),
        );
        assert_eq!(
            agent_status(&Cancel::new(), &local_worker()).unwrap_err(),
            failure(MISSING_TMUX, "").1,
        );
        let tmux = root.path().join("tmux");
        fs::write(
            &tmux,
            r#"#!/bin/sh
if [ "$*" = '-N -V' ]; then printf 'tmux 3.2a\n'; exit; fi
shift 4
case "$1" in
    display-message)
        if [ "$5" = '#{pane_height}' ]; then
            printf '24\n'
        else
            printf '1\n'
        fi
        ;;
    capture-pane) printf 'local marker\n\n' ;;
    list-sessions) printf '$7 1234 1700000000 1700000001 1\n' ;;
    list-windows) printf '2\n' ;;
    *) exit 99 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            preview(&Cancel::new(), &local_worker(), &session())
                .unwrap()
                .text,
            "local marker",
        );
    }
}
