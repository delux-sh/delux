use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::cancel::Cancel;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Set(&'static str),
    Clear,
}

pub(super) fn mapping(agent: &str, event: &str, payload: &Value) -> Option<Action> {
    use Action::{Clear, Set};
    match (agent, event) {
        ("claude" | "codex" | "gemini", "SessionStart" | "SessionEnd") | ("codex", "Interrupt") => {
            Some(Clear)
        }
        ("claude" | "codex", "UserPromptSubmit" | "PostToolUse")
        | ("claude", "PostToolUseFailure") => Some(Set("working")),
        ("gemini", "BeforeAgent" | "AfterTool") => Some(Set("working")),
        ("gemini", "Notification") => Some(Set("waiting")),
        ("gemini", "AfterAgent") => Some(Set("done")),
        ("claude" | "codex", "PermissionRequest") | ("claude", "PreToolUse" | "Notification") => {
            Some(Set("waiting"))
        }
        ("claude", "Stop")
            if payload
                .get("background_tasks")
                .and_then(Value::as_array)
                .is_some_and(|tasks| !tasks.is_empty()) =>
        {
            Some(Set("working"))
        }
        ("claude" | "codex", "Stop") => Some(Set("done")),
        ("claude", "StopFailure") => Some(Set("error")),
        _ => None,
    }
}

pub(super) fn action(args: &[&str], payload: &Value) -> Option<Action> {
    let args = match args {
        [rest @ .., "--agent", name]
            if super::valid_name(name)
                && matches!(rest.first().copied(), Some("set" | "clear")) =>
        {
            rest
        }
        _ => args,
    };
    match args {
        ["set", "working"] => Some(Action::Set("working")),
        ["set", "waiting" | "blocked"] => Some(Action::Set("waiting")),
        ["set", "done"] => Some(Action::Set("done")),
        ["set", "error"] => Some(Action::Set("error")),
        ["clear"] => Some(Action::Clear),
        ["hook", agent, event] => mapping(agent, event, payload),
        _ => None,
    }
}

pub(super) fn command(pane: Option<&str>, action: Action, seconds: u64) -> Option<Vec<String>> {
    let pane = pane.filter(|pane| !pane.is_empty())?;
    let mut args = vec!["tmux".into(), "set-option".into(), "-p".into()];
    if action == Action::Clear {
        args.push("-u".into());
    }
    args.extend(["-t".into(), pane.into(), "@agent_status".into()]);
    if let Action::Set(state) = action {
        args.push(format!("{state} {seconds}"));
    }
    Some(args)
}

pub(crate) fn report(args: &[&str]) {
    let pane = std::env::var("TMUX_PANE").ok();
    if pane.as_deref().is_none_or(str::is_empty) {
        return;
    }
    let payload = if args == ["hook", "claude", "Stop"] {
        stdin_payload()
    } else {
        Value::Null
    };
    let Some(action) = action(args, &payload) else {
        return;
    };
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let Some(command) = command(pane.as_deref(), action, seconds) else {
        return;
    };
    // Direct PATH lookup, like local `command tmux`, retains TMUX's server choice.
    // Never interpolate either the pane or hook input into a shell command.
    let cancel = Cancel::from_flag(Arc::new(AtomicBool::new(false)));
    let _ = crate::process::run_command(&cancel, &command, Duration::from_secs(2), false);
}

const PAYLOAD_LIMIT: usize = 1024 * 1024;

pub(super) fn payload(bytes: &[u8]) -> Value {
    if bytes.len() > PAYLOAD_LIMIT {
        return Value::Null;
    }
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

fn stdin_payload() -> Value {
    let deadline = Instant::now() + Duration::from_millis(200);
    let input = rustix::stdio::stdin();
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Value::Null;
        }
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: remaining.as_nanos() as _,
        };
        let mut poll = [rustix::event::PollFd::new(
            &input,
            rustix::event::PollFlags::IN,
        )];
        match rustix::event::poll(&mut poll, Some(&timeout)) {
            Ok(0) => return Value::Null,
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return Value::Null,
        }
        match rustix::io::read(input, &mut buffer) {
            Ok(0) => return payload(&raw),
            Ok(count) => {
                if count > PAYLOAD_LIMIT - raw.len() {
                    return Value::Null;
                }
                raw.extend_from_slice(&buffer[..count]);
            }
            Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => continue,
            Err(_) => return Value::Null,
        }
    }
}
