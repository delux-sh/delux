use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::collections::{HashMap, HashSet};

use crate::exit_codes::SESSION_CHANGED;

// Derived ordering is pane urgency, from least to most urgent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Done,
    Working,
    Error,
    Waiting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct AgentStatus {
    pub state: AgentState,
    pub since: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatusRow {
    pub server_pid: i64,
    pub server_start: i64,
    pub session_id: String,
    pub output: i64,
    pub agent: Option<AgentStatus>,
}

pub fn parse_agent_status(value: &str) -> Option<AgentStatus> {
    let (state, since) = match value.split_once(' ') {
        Some((state, epoch)) => {
            if epoch.len() > 12 || !tmux_digits(epoch) {
                return None;
            }
            let epoch = epoch.parse::<i64>().ok()?;
            if epoch == 0 {
                return None;
            }
            (state, Some(epoch))
        }
        None => (value, None),
    };
    let state = match state {
        "working" => AgentState::Working,
        "waiting" | "blocked" => AgentState::Waiting,
        "done" => AgentState::Done,
        "error" => AgentState::Error,
        "idle" => return None,
        _ => return None,
    };
    Some(AgentStatus { state, since })
}

fn merge_agent_status(current: &mut Option<AgentStatus>, next: Option<AgentStatus>) {
    if let Some(next) = next
        && current.is_none_or(|old| (next.state, next.since) > (old.state, old.since))
    {
        *current = Some(next);
    }
}

/// Display metadata and the lifetime identity used when attaching.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub windows: i64,
    pub attached: i64,
    pub server_pid: i64,
    pub server_start: i64,
    pub socket_path: String,
    pub created: i64,
    /// When someone last used the session: typed into it from a client, or
    /// attached. Program output does not count; see `output`.
    pub activity: i64,
    /// When any of the session's windows last produced output.
    pub output: i64,
    pub command: String,
    pub path: String,
    pub agent: Option<AgentStatus>,
}

/// Explicit session creation; directory expansion is limited to the target HOME.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSession {
    pub name: String,
    pub directory: String,
    pub command: String,
}

impl NewSession {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Enter a session name".into());
        }
        if self.name.chars().count() > 64
            || self
                .name
                .chars()
                .any(|c| c.is_control() || c == '.' || c == ':')
        {
            return Err(
                "Session names must be at most 64 characters, without controls, '.' or ':'".into(),
            );
        }
        for (label, value) in [("Directory", &self.directory), ("Command", &self.command)] {
            if value.chars().count() > 4096 || value.chars().any(char::is_control) {
                return Err(format!(
                    "{label} must be at most 4096 characters without controls"
                ));
            }
        }
        if !self.directory.is_empty()
            && self.directory != "~"
            && !self.directory.starts_with("~/")
            && !self.directory.starts_with('/')
        {
            return Err("Directory must be an absolute path, ~, or ~/path".into());
        }
        Ok(())
    }
}

// -N was added in tmux 3.2. Probe it without connecting to a server; checking
// capability also accepts development and vendor version strings.
macro_rules! version_prelude {
    () => {
        r#"command -v tmux >/dev/null 2>&1 || exit 70
command tmux -N -V >/dev/null 2>&1 || {
    printf '%s\n' 'Delux requires tmux 3.2 or newer with -N support; upgrade tmux on this computer and retry.' >&2
    exit 71
}
"#
    };
}

// -u preserves Unicode metadata under the diagnostic-only C locale.
macro_rules! tmux_prelude {
    () => {
        concat!(
            "unset TMUX\nset -f\n",
            version_prelude!(),
            "tmux_cmd() { command tmux -u -N -L default \"$@\"; }\n",
        )
    };
}
pub const TMUX_PRELUDE: &str = tmux_prelude!();

// Shell exit codes are checked against crate::exit_codes by the script tests.
macro_rules! encoding_prelude {
    () => {
        r#"LC_ALL=C
export LC_ALL
command -v base64 >/dev/null 2>&1 || exit 72
command -v tr >/dev/null 2>&1 || exit 72
encode() {
    encoded=$(printf '%s' "$1" | base64) || exit 72
    printf '%s' "$encoded" | tr -d '\r\n' || exit 72
}
# Test the tools even when there are no sessions.
probe=$(encode 'delux') || exit 72
[ "$probe" = 'ZGVsdXg=' ] || exit 72
"#
    };
}

// Older servers crash when a missing target is formatted with session time
// keys. list-sessions formats only live sessions, so select the exact ID here.
macro_rules! session_times_prelude {
    () => {
        r#"session_times() {
    rows=$(tmux_cmd list-sessions -F '#{session_id} #{pid} #{start_time} #{session_created} #{session_activity}') || return 1
    while IFS= read -r row; do
        case "$row" in "$1 "*) printf '%s\n' "$row"; return 0 ;; esac
    done <<EOF
$rows
EOF
    return 1
}
"#
    };
}

// Reject unsafe option bytes before list-panes adds record delimiters. Encoding
// the complete list afterwards cannot distinguish injected newlines from panes.
macro_rules! agent_status_prelude {
    () => {
        "agent_status_format='#{?pane_program_state,#{pane_program_state},#{?#{m/r:[^a-z0-9 ],#{@agent_status}},,#{@agent_status}}}'\n"
    };
}

pub const INVENTORY_SCRIPT: &str = concat!(
    tmux_prelude!(),
    encoding_prelude!(),
    session_times_prelude!(),
    agent_status_prelude!(),
    r#"
emit() {
    encode "$1" || exit 72
    printf '%b' "$2"
}
field() {
    value=$(tmux_cmd display-message -p -t "$1" "$2" && printf '.') || exit 73
    value=${value%.}
    # Remove exactly display-message's newline, not data's trailing newlines.
    value=${value%?}
    emit "$value" "$3"
}
sessions=$(tmux_cmd list-sessions -F '#{session_id}' 2>&1)
status=$?
if [ "$status" -ne 0 ]; then
    case "$status:$sessions" in
        '1:no server running on '*|'1:error connecting to '*' (No such file or directory)')
            printf '%s\n' 'DELUX_SESSIONS_4' 'DELUX_END'
            exit 0
            ;;
        *) printf '%s\n' "$sessions" >&2; exit 73 ;;
    esac
fi
printf '%s\n' 'DELUX_SESSIONS_4'
for session in $sessions; do
    case "$session" in '$'*) digits=${session#?} ;; *) exit 73 ;; esac
    case "$digits" in ''|*[!0-9]*) exit 73 ;; esac
    target="$session:"
    before=$(session_times "$session") || exit 73
    set -- $before
    [ "$#" -eq 5 ] && [ "$1" = "$session" ] || exit 73
    before="$1 $2 $3 $4"
    server_pid=$2
    server_start=$3
    created=$4
    activity=$5
    emit "$session" '\t'
    field "$target" '#{session_name}' '\t'
    field "$target" '#{session_windows}' '\t'
    field "$target" '#{session_attached}' '\t'
    emit "$server_pid" '\t'
    emit "$server_start" '\t'
    field "$target" '#{socket_path}' '\t'
    emit "$created" '\t'
    emit "$activity" '\t'
    window_output=$(tmux_cmd list-windows -t "$target" -F '#{window_activity}') || exit 73
    output=0
    for value in $window_output; do
        case "$value" in ''|*[!0-9]*) exit 73 ;; esac
        [ "$value" -gt "$output" ] && output=$value
    done
    emit "$output" '\t'
    field "$target" '#{pane_current_command}' '\t'
    field "$target" '#{pane_current_path}' '\t'
    # Native status comes from the program itself and needs no installed hooks.
    pane_status=$(tmux_cmd list-panes -s -t "$target" -F "$agent_status_format") || exit 73
    emit "$pane_status" '\n'
    after=$(session_times "$session") || exit 73
    set -- $after
    [ "$#" -eq 5 ] && [ "$before" = "$1 $2 $3 $4" ] || exit 73
done
printf '%s\n' 'DELUX_END'
"#
);

pub const STATUS_SCRIPT: &str = concat!(
    tmux_prelude!(),
    agent_status_prelude!(),
    r##"
# Native status comes from the program itself and needs no installed hooks.
rows=$(tmux_cmd list-panes -a -F "#{pid} #{start_time} #{session_id} #{window_activity} $agent_status_format" 2>&1)
status=$?
if [ "$status" -ne 0 ]; then
    case "$status:$rows" in
        '1:no server running on '*|'1:error connecting to '*' (No such file or directory)')
            printf '%s\n' 'DELUX_STATUS_2' 'DELUX_END'
            exit 0
            ;;
        *) printf '%s\n' "$rows" >&2; exit 73 ;;
    esac
fi
printf '%s\n' 'DELUX_STATUS_2'
[ -z "$rows" ] || printf '%s\n' "$rows"
printf '%s\n' 'DELUX_END'
"##
);

/// Parse one complete pane-status frame, retaining empty session aggregates.
pub fn parse_status(output: &str) -> Result<Vec<AgentStatusRow>, String> {
    let malformed = || "Malformed or truncated tmux status".to_owned();
    let mut records = output.strip_suffix('\n').unwrap_or(output).split('\n');
    if records.next() != Some("DELUX_STATUS_2") || records.next_back() != Some("DELUX_END") {
        return Err(malformed());
    }
    let mut result: Vec<AgentStatusRow> = Vec::new();
    let mut indices = HashMap::new();
    for record in records {
        let mut fields = record.splitn(5, ' ');
        let pid = fields.next().ok_or_else(malformed)?;
        let start = fields.next().ok_or_else(malformed)?;
        let id = fields.next().ok_or_else(malformed)?;
        let output = fields.next().ok_or_else(malformed)?;
        if !tmux_digits(pid)
            || !tmux_digits(start)
            || !id.strip_prefix('$').is_some_and(tmux_digits)
            || !tmux_digits(output)
        {
            return Err(malformed());
        }
        let server_pid = pid.parse::<i64>().map_err(|_| malformed())?;
        let server_start = start.parse::<i64>().map_err(|_| malformed())?;
        let output = output.parse::<i64>().map_err(|_| malformed())?;
        if server_pid == 0 || server_start == 0 || output == 0 {
            return Err(malformed());
        }
        let agent = fields.next().and_then(parse_agent_status);
        if let Some(&index) = indices.get(&(server_pid, server_start, id)) {
            let row: &mut AgentStatusRow = &mut result[index];
            row.output = row.output.max(output);
            merge_agent_status(&mut row.agent, agent);
        } else {
            indices.insert((server_pid, server_start, id), result.len());
            result.push(AgentStatusRow {
                server_pid,
                server_start,
                session_id: id.into(),
                output,
                agent,
            });
        }
    }
    Ok(result)
}

fn tmux_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

pub fn validate_session(session: &Session) -> Result<(), String> {
    if !session.id.strip_prefix('$').is_some_and(tmux_digits) {
        return Err("Invalid tmux session ID".into());
    }
    if session.name.contains('\0')
        || session.socket_path.contains('\0')
        || session.command.contains('\0')
        || session.path.contains('\0')
    {
        return Err("Invalid tmux text field".into());
    }
    if session.socket_path.is_empty() {
        return Err("Missing tmux socket identity".into());
    }
    if session.windows < 1
        || session.attached < 0
        || session.server_pid < 1
        || session.server_start < 1
        || session.created < 1
        || session.activity < 1
        || session.output < 1
    {
        return Err("Invalid tmux numeric field".into());
    }
    Ok(())
}

fn decode_tmux_field(encoded: &str) -> Result<String, String> {
    let malformed = || "Malformed tmux encoded field".to_owned();
    let data = STANDARD.decode(encoded).map_err(|_| malformed())?;
    if STANDARD.encode(&data) != encoded {
        return Err(malformed());
    }
    let value = String::from_utf8(data).map_err(|_| malformed())?;
    if value.contains('\0') {
        return Err("NUL in tmux field".into());
    }
    Ok(value)
}

/// Accept exactly one complete frame, never partial or mixed data.
pub fn parse_inventory(output: &str) -> Result<Vec<Session>, String> {
    let records: Vec<_> = output
        .strip_suffix('\n')
        .unwrap_or(output)
        .split('\n')
        .collect();
    if records.len() < 2 || records[0] != "DELUX_SESSIONS_4" || records.last() != Some(&"DELUX_END")
    {
        return Err("Malformed or truncated tmux inventory".into());
    }
    let mut result: Vec<Session> = Vec::with_capacity(records.len() - 2);
    let mut seen = HashSet::with_capacity(records.len() - 2);
    for record in &records[1..records.len() - 1] {
        let fields: Vec<_> = record.split('\t').collect();
        if fields.len() != 13 {
            return Err("Malformed tmux inventory record".into());
        }
        let mut values = [const { String::new() }; 13];
        for (value, field) in values.iter_mut().zip(fields) {
            *value = decode_tmux_field(field)?;
        }
        let mut numbers = [0_i64; 7];
        for (number, index) in numbers.iter_mut().zip([2, 3, 4, 5, 7, 8, 9]) {
            if !tmux_digits(&values[index]) {
                return Err("Invalid tmux numeric field".into());
            }
            *number = values[index]
                .parse()
                .map_err(|_| "Invalid tmux numeric field".to_owned())?;
        }
        let [
            id,
            name,
            _,
            _,
            _,
            _,
            socket_path,
            _,
            _,
            _,
            command,
            path,
            panes,
        ] = values;
        let mut agent = None;
        for pane in panes.split('\n') {
            merge_agent_status(&mut agent, parse_agent_status(pane));
        }
        let session = Session {
            id,
            name,
            windows: numbers[0],
            attached: numbers[1],
            server_pid: numbers[2],
            server_start: numbers[3],
            socket_path,
            created: numbers[4],
            activity: numbers[5],
            output: numbers[6],
            command,
            path,
            agent,
        };
        validate_session(&session)?;
        if let Some(server) = result.first()
            && (session.server_pid != server.server_pid
                || session.server_start != server.server_start
                || session.socket_path != server.socket_path)
        {
            return Err("Mixed tmux server identities".into());
        }
        if !seen.insert(session.id.clone()) {
            return Err("Duplicate tmux session identity".into());
        }
        result.push(session);
    }
    Ok(result)
}

/// Quote a NUL-free string as one POSIX shell argument.
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

// tmux's argv parser treats a trailing semicolon as a command separator even
// when the surrounding shell quoted it. Escape that separate parsing layer.
fn creation_argument(value: &str) -> String {
    match value.strip_suffix(';') {
        Some(prefix) => shell_quote(&format!("{prefix}\\;")),
        None => shell_quote(value),
    }
}

/// Create once, capturing the identity directly from new-session, then inventory.
pub fn create_script(request: &NewSession) -> Result<String, String> {
    request.validate()?;
    let mut script = String::from(TMUX_PRELUDE);
    // Check tools before mutation without leaking the diagnostic locale into
    // a newly started server and every shell it subsequently creates.
    script.push_str("(\n");
    script.push_str(encoding_prelude!());
    script.push_str(") || exit $?\ndirectory=");
    script.push_str(&shell_quote(&request.directory));
    script.push_str(
        r#"
case "$directory" in
    ''|'~') directory=$HOME ;;
    '~/'*) directory=$HOME/${directory#??} ;;
esac
case "$directory" in
    /*) ;;
    *) printf '%s\n' 'The target computer HOME is not an absolute directory.' >&2; exit 1 ;;
esac
# Use the client's cwd, not -c: tmux expands formats in -c, including #(jobs).
# cd validates the directory before starting a server or creating a session.
if ! CDPATH= cd "$directory" 2>/dev/null; then
    printf '%s\n' 'The requested directory does not exist or is not accessible.' >&2
    exit 1
fi
created=$(command tmux -u -L default new-session -d -P -F '#{session_id} #{pid} #{start_time} #{session_created}' -s "#,
    );
    // Session names also undergo tmux format expansion.
    script.push_str(&creation_argument(&request.name.replace('#', "##")));
    script.push_str(" --");
    if !request.command.is_empty() {
        script.push(' ');
        script.push_str(&creation_argument(&request.command));
    }
    script.push_str(
        ") || exit $?\n\
         printf '%s\\n' 'DELUX_CREATED_1' \"$created\"\n",
    );
    script.push_str(INVENTORY_SCRIPT);
    Ok(script)
}

/// Never resolve a creation by name: it may already have exited and been replaced.
pub fn parse_created_session(output: &str) -> Result<(Session, Vec<Session>), String> {
    let invalid = || "Malformed or truncated tmux creation result".to_owned();
    let rest = output
        .strip_prefix("DELUX_CREATED_1\n")
        .ok_or_else(invalid)?;
    let (identity, inventory) = rest.split_once('\n').ok_or_else(invalid)?;
    let mut fields = identity.split(' ');
    let id = fields.next().ok_or_else(invalid)?;
    let mut number = || {
        let value = fields.next().ok_or_else(invalid)?;
        if !tmux_digits(value) {
            return Err(invalid());
        }
        value.parse::<i64>().map_err(|_| invalid())
    };
    let pid = number()?;
    let start = number()?;
    let created = number()?;
    if fields.next().is_some()
        || !id.strip_prefix('$').is_some_and(tmux_digits)
        || pid < 1
        || start < 1
        || created < 1
    {
        return Err(invalid());
    }
    let sessions = parse_inventory(inventory)?;
    let session = sessions
        .iter()
        .find(|session| {
            session.id == id
                && session.server_pid == pid
                && session.server_start == start
                && session.created == created
        })
        .ok_or_else(|| {
            "The created session already ended or changed; its command may have exited".to_owned()
        })?;
    Ok((session.clone(), sessions))
}

// Session IDs are monotonic within a server. Do not add session_created here:
// time keys on vanished targets can crash older servers, even with a new client.
fn identity_condition(session: &Session) -> String {
    let pid = session.server_pid.to_string();
    let start = session.server_start.to_string();
    let mut condition = String::new();
    for (key, value) in [
        ("session_id", session.id.as_str()),
        ("pid", &pid),
        ("start_time", &start),
        ("socket_path", &session.socket_path),
    ] {
        let escaped = value
            .replace('#', "##")
            .replace(',', "#,")
            .replace('}', "#}");
        let part = format!("#{{==:#{{{key}}},{escaped}}}");
        condition = if condition.is_empty() {
            part
        } else {
            format!("#{{&&:{condition},{part}}}")
        };
    }
    condition
}

/// Guard and attach in one synchronous tmux command queue, without a fallback.
pub fn attach_script(session: &Session, embedded: bool) -> Result<String, String> {
    validate_session(session)?;
    let condition = identity_condition(session);
    let target = format!("{}:", session.id);
    let action = format!("attach-session -t {}", shell_quote(&session.id));
    let stale = format!(
        "display-message -p 'DELUX_STALE: selected session changed' ; run-shell 'exit {SESSION_CHANGED}'"
    );
    let mut script = String::from(TMUX_PRELUDE);
    script.push_str("exec tmux -u -N -L default");
    if embedded {
        // Advertise only what Delux's embedded terminal actually implements.
        script.push_str(" -T sync");
    }
    for argument in ["if-shell", "-F", "-t", &target, &condition, &action, &stale] {
        script.push(' ');
        script.push_str(&shell_quote(argument));
    }
    script.push('\n');
    Ok(script)
}

/// Guard and switch the current pane's client in the existing tmux server.
pub fn switch_script(session: &Session) -> Result<String, String> {
    validate_session(session)?;
    let condition = identity_condition(session);
    let target = format!("{}:", session.id);
    let action = format!("switch-client -t {}", shell_quote(&session.id));
    let stale = format!(
        "display-message -p 'DELUX_STALE: selected session changed' ; run-shell 'exit {SESSION_CHANGED}'"
    );
    // Socket paths may contain commas; only the final PID and index are removed.
    // Preserve TMUX and TMUX_PANE so tmux can identify the invoking pane's client.
    let mut script = String::from(version_prelude!());
    script.push_str(
        "socket=${TMUX%,*}\n\
         socket=${socket%,*}\n\
         exec tmux -u -N -S \"$socket\"",
    );
    for argument in ["if-shell", "-F", "-t", &target, &condition, &action, &stale] {
        script.push(' ');
        script.push_str(&shell_quote(argument));
    }
    script.push('\n');
    Ok(script)
}

pub const PREVIEW_LINES: u16 = 200;

/// Capture the last PREVIEW_LINES rows of the active pane as joined text, including
/// tmux's SGR color and attribute sequences (`-e`); the picker honors only those.
/// Identity is checked before and after capture.
pub fn preview_script(session: &Session) -> Result<String, String> {
    validate_session(session)?;
    let mut script = String::from(TMUX_PRELUDE);
    script.push_str(&format!(
        "session={}\ntarget={}\nidentity={}\nlines={PREVIEW_LINES}\n",
        shell_quote(&session.id),
        shell_quote(&format!("{}:", session.id)),
        shell_quote(&identity_condition(session)),
    ));
    script.push_str(encoding_prelude!());
    script.push_str(session_times_prelude!());
    script.push_str(
        r#"
stale() {
    printf '%s\n' 'DELUX_STALE'
    exit 74
}
check_identity() {
    actual=$(tmux_cmd display-message -p -t "$target" "$identity") || stale
    [ "$actual" = 1 ] || stale
}
check_identity
height=$(tmux_cmd display-message -p -t "$target" '#{pane_height}')
status=$?
if [ "$status" -ne 0 ]; then
    check_identity
    exit 73
fi
case "$height" in ''|*[!0-9]*) exit 73 ;; esac
[ "$height" -ge 1 ] || exit 73
# Negative capture offsets count backwards from the first visible row.
start=$((height - lines))
capture=$(tmux_cmd capture-pane -p -e -J -S "$start" -t "$target" && printf '.')
status=$?
times=$(session_times "$session") || stale
set -- $times
[ "$#" -eq 5 ] || exit 73
activity=$5
case "$activity" in ''|*[!0-9]*) exit 73 ;; esac
window_output=$(tmux_cmd list-windows -t "$target" -F '#{window_activity}') || exit 73
output=0
for value in $window_output; do
    case "$value" in ''|*[!0-9]*) exit 73 ;; esac
    [ "$value" -gt "$output" ] && output=$value
done
check_identity
[ "$status" -eq 0 ] || exit 73
# Preserve captured trailing newlines while removing only our sentinel.
capture=${capture%.}
encoded=$(encode "$capture") || exit 72
printf '%s\n' 'DELUX_PREVIEW_2' "$activity" "$output" "$encoded" 'DELUX_END'
"#,
    );
    Ok(script)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub text: String,
    pub activity: i64,
    pub output: i64,
}

/// Decode exactly one preview frame, replacing invalid UTF-8 and trimming
/// trailing lines that show nothing (whitespace and escape sequences only).
pub fn parse_preview(output: &str) -> Result<Preview, String> {
    let malformed = || "Malformed or truncated tmux preview".to_owned();
    let mut records = output.strip_suffix('\n').unwrap_or(output).split('\n');
    if records.next() != Some("DELUX_PREVIEW_2") {
        return Err(malformed());
    }
    let mut timestamp = || {
        let value = records.next().ok_or_else(malformed)?;
        if !tmux_digits(value) {
            return Err(malformed());
        }
        value.parse::<i64>().map_err(|_| malformed())
    };
    let activity = timestamp()?;
    let output = timestamp()?;
    if activity < 1 || output < 1 {
        return Err(malformed());
    }
    let encoded = records.next().ok_or_else(malformed)?;
    if records.next() != Some("DELUX_END") || records.next().is_some() {
        return Err(malformed());
    }
    let bytes = STANDARD.decode(encoded).map_err(|_| malformed())?;
    if STANDARD.encode(&bytes) != encoded {
        return Err(malformed());
    }
    let mut preview = String::from_utf8_lossy(&bytes).into_owned();
    let mut trimmed = preview.trim_end_matches('\n');
    while !trimmed.is_empty() {
        let (prefix, line) = trimmed.rsplit_once('\n').unwrap_or(("", trimmed));
        if !crate::ansi::is_blank(line) {
            break;
        }
        trimmed = prefix;
    }
    let length = trimmed.len();
    preview.truncate(length);
    Ok(Preview {
        text: preview,
        activity,
        output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TempDir, env_read, open_pty};
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::fs::{self, File};
    use std::io::{Read, Seek, SeekFrom};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::UnixDatagram;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::thread;
    use std::time::{Duration, Instant};

    type Environment = BTreeMap<OsString, OsString>;

    fn sample() -> Session {
        Session {
            id: "$7".into(),
            name: "work".into(),
            windows: 2,
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

    fn fields(s: &Session) -> Vec<String> {
        vec![
            s.id.clone(),
            s.name.clone(),
            s.windows.to_string(),
            s.attached.to_string(),
            s.server_pid.to_string(),
            s.server_start.to_string(),
            s.socket_path.clone(),
            s.created.to_string(),
            s.activity.to_string(),
            s.output.to_string(),
            s.command.clone(),
            s.path.clone(),
            s.agent.map_or_else(String::new, |agent| {
                let state = match agent.state {
                    AgentState::Working => "working",
                    AgentState::Waiting => "waiting",
                    AgentState::Done => "done",
                    AgentState::Error => "error",
                };
                agent
                    .since
                    .map_or_else(|| state.into(), |since| format!("{state} {since}"))
            }),
        ]
    }

    fn encoded_frame(fields: &[String]) -> String {
        format!("DELUX_SESSIONS_4\n{}\nDELUX_END\n", fields.join("\t"))
    }

    fn frame(sessions: &[Session]) -> String {
        let mut result = String::from("DELUX_SESSIONS_4\n");
        for session in sessions {
            result.push_str(
                &fields(session)
                    .iter()
                    .map(|s| STANDARD.encode(s))
                    .collect::<Vec<_>>()
                    .join("\t"),
            );
            result.push('\n');
        }
        result.push_str("DELUX_END\n");
        result
    }

    #[test]
    fn agent_status_grammar_table() {
        for (value, expected) in [
            ("working", Some((AgentState::Working, None))),
            ("waiting 1", Some((AgentState::Waiting, Some(1)))),
            ("blocked", Some((AgentState::Waiting, None))),
            ("blocked 1", Some((AgentState::Waiting, Some(1)))),
            ("error", Some((AgentState::Error, None))),
            ("error 1", Some((AgentState::Error, Some(1)))),
            (
                "done 999999999999",
                Some((AgentState::Done, Some(999999999999))),
            ),
            ("working 000000000001", Some((AgentState::Working, Some(1)))),
            ("", None),
            ("Working", None),
            ("idle", None),
            ("idle 1", None),
            ("done ", None),
            (" done", None),
            ("done 0", None),
            ("done 000", None),
            ("done 1234567890123", None),
            ("done +1", None),
            ("done -1", None),
            ("done １", None),
            ("done ١", None),
            ("done  1", None),
            ("done\t1", None),
            ("done 1 ", None),
            ("done 1\n", None),
            ("done 1 2", None),
        ] {
            assert_eq!(
                parse_agent_status(value),
                expected.map(|(state, since)| AgentStatus { state, since }),
                "{value:?}"
            );
        }
    }

    #[test]
    fn inventory_aggregates_all_panes_and_rejects_twelve_fields() {
        for (panes, expected) in [
            (
                "done 90\nworking 20\nerror 99\nwaiting 1\nwaiting 3\nwaiting",
                "waiting 3",
            ),
            ("waiting\nworking 999", "waiting"),
            ("done 90\nworking\nworking 2", "working 2"),
            ("done\ndone 3\ndone 1", "done 3"),
            ("working 999\nerror\nerror 2\ndone 999", "error 2"),
            ("error 999\nwaiting", "waiting"),
            ("waiting\nerror 999", "waiting"),
            ("error\nworking 999", "error"),
            ("idle\nworking\nidle 1", "working"),
            ("done\nidle\nidle 1", "done"),
            ("idle\nidle 1", ""),
            ("blocked\nerror\nidle", "waiting"),
            ("idle\nerror\nidle", "error"),
            ("invalid\nworking 0\n", ""),
        ] {
            let mut values = fields(&sample());
            values[12] = panes.into();
            let encoded: Vec<_> = values.iter().map(|v| STANDARD.encode(v)).collect();
            assert_eq!(
                parse_inventory(&encoded_frame(&encoded)).unwrap()[0].agent,
                parse_agent_status(expected)
            );
            assert!(parse_inventory(&encoded_frame(&encoded[..12])).is_err());
        }
        let mut session = sample();
        session.agent = parse_agent_status("working 42");
        assert_eq!(
            parse_inventory(&frame(&[session.clone()])).unwrap(),
            vec![session]
        );
    }

    #[test]
    fn status_aggregates_by_session_and_server_identity() {
        let output = "DELUX_STATUS_2\n1 2 $0 10 done 99\n1 2 $0 20 working 5\n1 2 $0 15 waiting 1\n1 2 $0 12 waiting 8\n1 2 $0 1 waiting\n1 2 $1 1 invalid\n2 2 $0 1 working\n1 3 $0 1 done\nDELUX_END\n";
        for output in [output, output.strip_suffix('\n').unwrap()] {
            let rows = parse_status(output).unwrap();
            assert_eq!(rows.len(), 4);
            assert_eq!(
                rows[0],
                AgentStatusRow {
                    server_pid: 1,
                    server_start: 2,
                    session_id: "$0".into(),
                    output: 20,
                    agent: parse_agent_status("waiting 8")
                }
            );
            assert_eq!(rows[1].agent, None);
            assert_eq!(rows[2].agent, parse_agent_status("working"));
            assert_eq!(rows[3].agent, parse_agent_status("done"));
        }
        assert!(
            parse_status("DELUX_STATUS_2\nDELUX_END\n")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            parse_status("DELUX_STATUS_2\n1 2 $0 1 \nDELUX_END").unwrap()[0].agent,
            None
        );
    }

    #[test]
    fn status_parses_native_states_without_timestamps() {
        let output = concat!(
            "DELUX_STATUS_2\n",
            "1 2 $0 1 error\n",
            "1 2 $0 1 blocked\n",
            "1 2 $0 1 idle\n",
            "1 2 $1 1 idle\n",
            "1 2 $1 1 working\n",
            "1 2 $1 1 error\n",
            "1 2 $1 1 done\n",
            "1 2 $2 1 idle\n",
            "DELUX_END\n",
        );
        let rows = parse_status(output).unwrap();
        assert_eq!(rows.len(), 3);
        for (row, state) in
            rows.iter()
                .zip([Some(AgentState::Waiting), Some(AgentState::Error), None])
        {
            assert_eq!(
                row.agent,
                state.map(|state| AgentStatus { state, since: None })
            );
        }
    }

    #[test]
    fn status_rejects_incomplete_frames_and_invalid_identities() {
        let good = "DELUX_STATUS_2\n1 2 $0 1 working\nDELUX_END\n";
        for output in [
            String::new(),
            "DELUX_STATUS_2".into(),
            good.replace("DELUX_END\n", ""),
            format!("{good}{good}"),
            format!("banner\n{good}"),
            format!("{good}noise\n"),
            format!("{good}\n"),
            good.replace('\n', "\r\n"),
            good.replace("STATUS_2", "STATUS_1"),
        ] {
            assert!(parse_status(&output).is_err(), "{output:?}");
        }
        for row in [
            "",
            "1",
            "1 2",
            "1 2 $",
            "1 2 0",
            "-1 2 $0",
            "+1 2 $0",
            "1 ٢ $0",
            "1 2 $１",
            "1  2 $0",
            "1\t2 $0",
            "9223372036854775808 2 $0",
            "1 9223372036854775808 $0",
            "0 2 $0 1 working",
            "1 0 $0 1 working",
            "1 2 $0 0 working",
            "1 2 $0 -1 working",
            "1 2 $0 9223372036854775808 working",
            "1 2 $0 working",
            "noise",
            "DELUX_END",
        ] {
            let output = format!("DELUX_STATUS_2\n1 2 $0 1 done\n{row}\nDELUX_END\n");
            assert!(parse_status(&output).is_err(), "{row:?}");
        }
        for value in ["garbage", "working 0", "waiting  1", "done\t9", "done 1\r"] {
            let output = format!("DELUX_STATUS_2\n1 2 $0 1 {value}\nDELUX_END\n");
            assert_eq!(parse_status(&output).unwrap()[0].agent, None);
        }
    }

    #[test]
    fn creation_result_requires_one_complete_frame_and_matching_lifetime() {
        let created = sample();
        let identity = format!(
            "{} {} {} {}",
            created.id, created.server_pid, created.server_start, created.created
        );
        let inventory = frame(std::slice::from_ref(&created));
        let complete = format!("DELUX_CREATED_1\n{identity}\n{inventory}");
        assert_eq!(
            parse_created_session(&complete),
            Ok((created.clone(), vec![created.clone()]))
        );
        for invalid in [
            String::new(),
            format!("banner\n{complete}"),
            format!("{complete}{complete}"),
            format!("{complete}trailer\n"),
            format!("DELUX_CREATED_1\n{identity}\n"),
            format!("DELUX_CREATED_1\n{identity} extra\n{inventory}"),
            format!("DELUX_CREATED_1\n$7 0 1700000000 1700000001\n{inventory}"),
            format!("DELUX_CREATED_1\n$7 +1234 1700000000 1700000001\n{inventory}"),
        ] {
            assert!(parse_created_session(&invalid).is_err(), "{invalid:?}");
        }
        for field in 0..4 {
            let mut replacement = created.clone();
            match field {
                0 => replacement.id = "$8".into(),
                1 => replacement.server_pid += 1,
                2 => replacement.server_start += 1,
                _ => replacement.created += 1,
            }
            let replaced = format!("DELUX_CREATED_1\n{identity}\n{}", frame(&[replacement]));
            assert!(parse_created_session(&replaced).is_err(), "{field}");
        }
    }

    #[test]
    fn new_session_validates_names_paths_and_commands() {
        let valid = NewSession {
            name: "雪 ; '$HOME' #{pid}".into(),
            directory: "~/literal $HOME".into(),
            command: "printf '%s' \"ready\"; exec sleep 120".into(),
        };
        assert!(valid.validate().is_ok());
        for name in [
            "",
            " \t",
            "name.part",
            "name:part",
            "bad\nname",
            "bad\u{7f}",
        ] {
            let mut request = valid.clone();
            request.name = name.into();
            assert!(request.validate().is_err(), "{name:?}");
        }
        for directory in ["relative", "~someone", "$HOME", "~/bad\npath"] {
            let mut request = valid.clone();
            request.directory = directory.into();
            assert!(request.validate().is_err(), "{directory:?}");
        }
        for directory in ["", "~", "~/project", "/tmp/literal #{pid};"] {
            let mut request = valid.clone();
            request.directory = directory.into();
            assert!(request.validate().is_ok(), "{directory:?}");
        }
        let mut request = valid.clone();
        request.name = "雪".repeat(64);
        assert!(request.validate().is_ok());
        request.name.push('a');
        assert!(request.validate().is_err());
        for field in [0, 1] {
            let mut request = valid.clone();
            let value = if field == 0 {
                &mut request.directory
            } else {
                &mut request.command
            };
            *value = format!("/{}", "a".repeat(4096));
            assert!(request.validate().is_err());
        }
        for command in ["bad\0command", "bad\rcommand", "bad\u{1b}command"] {
            let mut request = valid.clone();
            request.command = command.into();
            assert!(request.validate().is_err());
        }
    }

    #[test]
    fn inventory_round_trips_sessions_and_hostile_metadata() {
        let mut other = sample();
        other.id = "$12".into();
        other.name = "second".into();
        other.attached = 3;
        other.command.clear();
        other.path.clear();
        let mut hostile = sample();
        hostile.name = "tab\tline\n雪 ' \\ #{pid},}\n\n".into();
        hostile.socket_path = "/tmp/\t雪\n'#{pid},}/default\n\n".into();
        hostile.command = "program\t雪\n\n".into();
        hostile.path = "/tmp/雪\t' #{pid}\n\n".into();
        for sessions in [vec![], vec![sample(), other], vec![hostile]] {
            let output = frame(&sessions);
            for output in [output.as_str(), output.strip_suffix('\n').unwrap()] {
                assert_eq!(parse_inventory(output).unwrap(), sessions);
            }
        }
    }

    #[test]
    fn inventory_rejects_malformed_frames_and_fields() {
        let good = frame(&[sample()]);
        let mut bad = vec![
            String::new(),
            "DELUX_END\n".into(),
            good.replace("DELUX_END\n", ""),
            format!("{good}\n"),
            format!("banner\n{good}"),
            format!("{good}noise\n"),
            good.replace("DELUX_SESSIONS_4", "DELUX_INVENTORY_1"),
            good.replace("DELUX_SESSIONS_4", "DELUX_SESSIONS_2"),
            good.replace("DELUX_SESSIONS_4", "DELUX_SESSIONS_3"),
            "DELUX_SESSIONS_1\nDELUX_END\n".into(),
            good.replacen('\t', "", 1),
            good.replacen('\t', "\t\t", 1),
            good.replace('\n', "\r\n"),
        ];
        for encoded in [
            "!", "YQ", "YQ===", "YR==", "Y Q==", "é===", "/w==", "AA==", "YQ==\r",
        ] {
            let mut values: Vec<_> = good
                .split('\n')
                .nth(1)
                .unwrap()
                .split('\t')
                .map(str::to_owned)
                .collect();
            values[1] = encoded.into();
            bad.push(encoded_frame(&values));
        }
        for (index, value) in [
            (0, "work"),
            (0, "$7; kill-server"),
            (0, "$٧"),
            (0, "$"),
            (2, "0"),
            (3, "-1"),
            (4, "0"),
            (5, "0"),
            (7, "0"),
            (8, "0"),
            (8, "-1"),
            (2, "+1"),
            (3, " 1"),
            (7, "１"),
            (6, ""),
            (6, "/tmp/\0socket"),
            (1, "nul\0name"),
            (9, "nul\0command"),
            (10, "nul\0path"),
            (2, "99999999999999999999999999999999"),
            (5, "9223372036854775808"),
        ] {
            let mut values = fields(&sample());
            values[index] = value.into();
            bad.push(encoded_frame(
                &values
                    .iter()
                    .map(|s| STANDARD.encode(s))
                    .collect::<Vec<_>>(),
            ));
        }
        for output in bad {
            assert!(parse_inventory(&output).is_err(), "accepted {output:?}");
        }
    }

    #[test]
    fn session_identity_rejects_mixed_servers_and_invalid_fields() {
        for mutate in [
            (|s: &mut Session| s.server_pid += 1) as fn(&mut Session),
            |s| s.server_start += 1,
            |s| s.socket_path.push_str("other"),
        ] {
            let mut other = sample();
            other.id = "$8".into();
            mutate(&mut other);
            assert!(parse_inventory(&frame(&[sample(), other])).is_err());
        }
        assert!(parse_inventory(&frame(&[sample(), sample()])).is_err());
        for mutate in [
            (|s: &mut Session| s.id = "$7; new-session".into()) as fn(&mut Session),
            |s| s.name = "\0".into(),
            |s| s.socket_path.clear(),
            |s| s.windows = 0,
            |s| s.attached = -1,
            |s| s.server_pid = 0,
            |s| s.server_start = 0,
            |s| s.created = 0,
            |s| s.activity = 0,
            |s| s.activity = -1,
            |s| s.output = 0,
            |s| s.command = "\0".into(),
            |s| s.path = "\0".into(),
        ] {
            let mut session = sample();
            mutate(&mut session);
            assert!(attach_script(&session, false).is_err());
            assert!(switch_script(&session).is_err());
            assert!(preview_script(&session).is_err());
        }
        // Invalid UTF-8 cannot inhabit Session's String fields. Exercise the
        // corresponding wire fields at the decoding boundary instead.
        for index in [1, 6, 9, 10] {
            let mut values: Vec<_> = fields(&sample())
                .iter()
                .map(|s| STANDARD.encode(s))
                .collect();
            values[index] = STANDARD.encode([0xff]);
            assert!(parse_inventory(&encoded_frame(&values)).is_err());
        }
    }

    fn preview_frame(bytes: &[u8]) -> String {
        format!(
            "DELUX_PREVIEW_2\n100\n200\n{}\nDELUX_END\n",
            STANDARD.encode(bytes)
        )
    }

    #[test]
    fn preview_decodes_text_and_trims_blank_lines() {
        for (bytes, expected) in [
            (&b""[..], ""),
            (&b"\n \t\n\n"[..], ""),
            (&b"first\n\n  last  \n \t\n\n"[..], "first\n\n  last  "),
            (&b"invalid \xff\n"[..], "invalid \u{fffd}"),
            ("雪\nsecond\n".as_bytes(), "雪\nsecond"),
        ] {
            let frame = preview_frame(bytes);
            for output in [frame.as_str(), frame.strip_suffix('\n').unwrap()] {
                let preview = parse_preview(output).unwrap();
                assert_eq!(preview.text, expected);
                assert_eq!((preview.activity, preview.output), (100, 200));
            }
        }
    }

    #[test]
    fn preview_rejects_malformed_frames() {
        let good = preview_frame(b"marker\n");
        for output in [
            String::new(),
            "DELUX_PREVIEW_2\nDELUX_END\n".into(),
            good.replace("DELUX_PREVIEW_2", "DELUX_PREVIEW_1"),
            good.replace("DELUX_END\n", ""),
            format!("banner\n{good}"),
            format!("{good}noise\n"),
            format!("{good}\n"),
            format!("{good}{good}"),
            good.replace('\n', "\r\n"),
            good.replace("\n100\n", "\n-1\n"),
            good.replace("\n100\n", "\n+1\n"),
            good.replace("\n100\n", "\n0\n"),
            good.replace("\n200\n", "\n\n"),
            good.replace("\n200\n", "\n1x\n"),
            good.replace("\n200\n", "\n9223372036854775808\n"),
        ] {
            assert!(parse_preview(&output).is_err(), "accepted {output:?}");
        }
        for encoded in ["!", "YQ", "YQ===", "YR==", "Y Q==", "é===", "YQ==\r"] {
            let output = format!("DELUX_PREVIEW_2\n100\n200\n{encoded}\nDELUX_END\n");
            assert!(parse_preview(&output).is_err(), "accepted {output:?}");
        }
    }

    // Callers hold env_read for their entire test, including fixture cleanup.
    fn environment(overrides: &[(&str, &OsStr)]) -> Environment {
        let mut env: Environment = std::env::vars_os()
            .filter(|(k, _)| k != "TMUX" && k != "TMUX_PANE")
            .collect();
        for (key, value) in overrides {
            env.insert((*key).into(), (*value).into());
        }
        env
    }

    fn which(name: &str) -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(name))
            .find(|path| {
                path.is_file()
                    && fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            })
    }

    #[derive(Debug)]
    struct RunResult {
        stdout: String,
        stderr: String,
        status: i32,
    }

    fn kill_group(pid: u32) {
        // SAFETY: negative PID addresses only this spawned child's process group.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }

    fn run(env: &Environment, name: impl AsRef<OsStr>, arguments: &[&str]) -> RunResult {
        // Files avoid pipe-reader hangs when a descendant retains an output fd.
        let tmp = TempDir::new();
        let mut stdout = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(tmp.path().join("stdout"))
            .unwrap();
        let mut stderr = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(tmp.path().join("stderr"))
            .unwrap();
        let mut command = Command::new(name);
        command
            .args(arguments)
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap());
        // SAFETY: setpgid is async-signal-safe and touches no Rust shared state.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                kill_group(child.id());
                let _ = child.wait();
                panic!("command timed out: {arguments:?}");
            }
            thread::sleep(Duration::from_millis(5));
        };
        stdout.seek(SeekFrom::Start(0)).unwrap();
        stderr.seek(SeekFrom::Start(0)).unwrap();
        let mut out = String::new();
        let mut err = String::new();
        stdout.read_to_string(&mut out).unwrap();
        stderr.read_to_string(&mut err).unwrap();
        RunResult {
            stdout: out,
            stderr: err,
            status: status.code().unwrap_or(-1),
        }
    }

    #[test]
    fn shell_quoting_preserves_literal_arguments() {
        let _env = env_read();
        let env = environment(&[]);
        for value in [
            "",
            "safe-path_123",
            "雪\t\n'\\\"$() ; #{pid},}\n\n",
            "$(exit 19)",
            "a'b'c",
        ] {
            let result = run(
                &env,
                "/bin/sh",
                &["-c", &format!("printf '%s' {}", shell_quote(value))],
            );
            assert_eq!(result.status, 0, "{result:?}");
            assert_eq!(result.stdout, value);
        }
    }

    #[test]
    fn switch_script_preserves_pane_environment_and_current_socket() {
        let _env = env_read();
        let root = TempDir::new();
        let tmux = root.path().join("tmux");
        fs::write(
            &tmux,
            "#!/bin/sh\nif [ \"$*\" = '-N -V' ]; then echo 'tmux 3.2a'; exit; fi\n\
             printf '%s\\n' \"$TMUX\" \"$TMUX_PANE\" \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = "/tmp/雪 ' #{pid},},more\n/socket";
        let current = format!("{socket},1234,7");
        let env = environment(&[
            ("PATH", root.path().as_os_str()),
            ("TMUX", OsStr::new(&current)),
            ("TMUX_PANE", OsStr::new("%42")),
        ]);
        let session = sample();
        let result = run(&env, "/bin/sh", &["-c", &switch_script(&session).unwrap()]);
        assert_eq!(result.status, 0, "{result:?}");
        let condition = identity_condition(&session);
        assert_eq!(
            result.stdout,
            format!(
                "{}\n",
                [
                    current.as_str(),
                    "%42",
                    "-u",
                    "-N",
                    "-S",
                    socket,
                    "if-shell",
                    "-F",
                    "-t",
                    "$7:",
                    &condition,
                    "switch-client -t '$7'",
                    "display-message -p 'DELUX_STALE: selected session changed' ; run-shell 'exit 74'",
                ]
                .join("\n")
            )
        );
    }

    #[test]
    fn unsupported_tmux_never_contacts_server_and_supported_no_server_is_empty() {
        let _env = env_read();
        let root = TempDir::new();
        for name in ["base64", "tr"] {
            let Some(path) = which(name) else {
                eprintln!("skipping: {name} unavailable");
                return;
            };
            symlink(path, root.path().join(name)).unwrap();
        }
        let tmux = root.path().join("tmux");
        fs::write(
            &tmux,
            r#"#!/bin/sh
if [ "$*" = '-N -V' ]; then
    printf '%s\n' "$DELUX_TEST_VERSION"
    exit "$DELUX_TEST_CAPABILITY"
fi
: > "$DELUX_TEST_MARKER"
printf '%s\n' 'no server running on /tmp/delux-test/default' >&2
exit 1
"#,
        )
        .unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = root.path().join("server-contacted");
        let attachment = attach_script(&sample(), false).unwrap();
        let switching = switch_script(&sample()).unwrap();
        let preview = preview_script(&sample()).unwrap();
        let env = environment(&[
            ("PATH", root.path().as_os_str()),
            ("DELUX_TEST_MARKER", marker.as_os_str()),
            ("DELUX_TEST_VERSION", OsStr::new("tmux 3.1c")),
            ("DELUX_TEST_CAPABILITY", OsStr::new("1")),
        ]);
        for script in [INVENTORY_SCRIPT, &attachment, &switching, &preview] {
            let result = run(&env, "/bin/sh", &["-c", script]);
            assert_eq!(
                result.status,
                crate::exit_codes::UNSUPPORTED_TMUX,
                "{}",
                result.stderr
            );
            assert!(!marker.exists(), "{result:?}");
        }
        let env = environment(&[
            ("PATH", root.path().as_os_str()),
            ("DELUX_TEST_MARKER", marker.as_os_str()),
            ("DELUX_TEST_VERSION", OsStr::new("tmux next-3.6")),
            ("DELUX_TEST_CAPABILITY", OsStr::new("0")),
        ]);
        let result = run(&env, "/bin/sh", &["-c", INVENTORY_SCRIPT]);
        assert_eq!(result.status, 0, "{result:?}");
        assert!(parse_inventory(&result.stdout).unwrap().is_empty());
        assert!(marker.exists());
    }

    #[test]
    fn inventory_handles_disappearing_sessions_without_targeted_time_lookups() {
        let _env = env_read();
        let root = TempDir::new();
        for name in ["base64", "tr"] {
            let Some(path) = which(name) else {
                eprintln!("skipping: {name} unavailable");
                return;
            };
            symlink(path, root.path().join(name)).unwrap();
        }
        let tmux = root.path().join("tmux");
        fs::write(
            &tmux,
            r#"#!/bin/sh
if [ "$*" = '-N -V' ]; then printf 'tmux 3.2a\n'; exit; fi
shift 4
case "$1" in
    list-sessions)
        if [ "$3" = '#{session_id}' ]; then
            printf '$7\n'
        else
            printf '$70 1234 1700000000 1700000001 99\n'
            if [ "$DELUX_TEST_MODE" != vanished-before ] &&
               { [ "$DELUX_TEST_MODE" != vanished-after ] || [ ! -f "$DELUX_TEST_MARKER" ]; }; then
                printf '$7 1234 1700000000 1700000001 1700000002\n'
            fi
        fi
        ;;
    display-message)
        case "$*" in
            *session_created*|*session_activity*)
                : > "$DELUX_TEST_DANGEROUS"; exit 99 ;;
        esac
        case "$5" in
            '#{session_name}') printf 'work\n' ;;
            '#{session_windows}') printf '2\n' ;;
            '#{session_attached}') printf '0\n' ;;
            '#{socket_path}') printf '/tmp/tmux-1000/default\n' ;;
            '#{pane_current_command}') printf 'sleep\n' ;;
            '#{pane_current_path}')
                : > "$DELUX_TEST_MARKER"; printf '/tmp/work\n' ;;
            *) exit 99 ;;
        esac
        ;;
    list-windows) printf '1700000003\n' ;;
    list-panes) printf '\n' ;;
    *) exit 99 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = root.path().join("read");
        let dangerous = root.path().join("dangerous");
        for mode in ["good", "vanished-before", "vanished-after"] {
            if marker.exists() {
                fs::remove_file(&marker).unwrap();
            }
            let env = environment(&[
                ("PATH", root.path().as_os_str()),
                ("DELUX_TEST_MODE", OsStr::new(mode)),
                ("DELUX_TEST_MARKER", marker.as_os_str()),
                ("DELUX_TEST_DANGEROUS", dangerous.as_os_str()),
            ]);
            let result = run(&env, "/bin/sh", &["-c", INVENTORY_SCRIPT]);
            assert!(!dangerous.exists(), "{mode}: {result:?}");
            if mode == "good" {
                assert_eq!(result.status, 0, "{result:?}");
                assert_eq!(parse_inventory(&result.stdout).unwrap(), vec![sample()]);
            } else {
                assert_eq!(result.status, crate::exit_codes::PROBE_FAILED, "{result:?}");
                assert!(parse_inventory(&result.stdout).is_err());
            }
        }
    }

    #[test]
    fn scripts_report_missing_and_broken_tools() {
        let _env = env_read();
        let root = TempDir::new();
        let env = environment(&[
            ("PATH", root.path().as_os_str()),
            ("TMUX_TMPDIR", root.path().as_os_str()),
        ]);
        let attachment = attach_script(&sample(), false).unwrap();
        let switching = switch_script(&sample()).unwrap();
        let preview = preview_script(&sample()).unwrap();
        for script in [INVENTORY_SCRIPT, &attachment, &switching, &preview] {
            let result = run(&env, "/bin/sh", &["-c", script]);
            assert_eq!(result.status, crate::exit_codes::MISSING_TMUX, "{result:?}");
        }
        let tmux = root.path().join("tmux");
        fs::write(
            &tmux,
            "#!/bin/sh\n[ \"$*\" = '-N -V' ] || exit 99\nprintf 'tmux 3.2a\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["base64", "tr"] {
            let Some(path) = which(name) else {
                eprintln!("skipping: {name} unavailable");
                return;
            };
            symlink(path, root.path().join(name)).unwrap();
        }
        for name in ["base64", "tr"] {
            let path = root.path().join(name);
            let original = fs::read_link(&path).unwrap();
            fs::remove_file(&path).unwrap();
            for script in [INVENTORY_SCRIPT, &preview] {
                let result = run(&env, "/bin/sh", &["-c", script]);
                assert_eq!(
                    result.status,
                    crate::exit_codes::MISSING_TOOLS,
                    "missing {name}: {result:?}"
                );
            }
            fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            for script in [INVENTORY_SCRIPT, &preview] {
                let result = run(&env, "/bin/sh", &["-c", script]);
                assert_eq!(
                    result.status,
                    crate::exit_codes::MISSING_TOOLS,
                    "broken {name}: {result:?}"
                );
            }
            fs::remove_file(&path).unwrap();
            symlink(original, &path).unwrap();
        }
    }

    #[test]
    fn preview_checks_identity_before_and_after_capture() {
        let _env = env_read();
        let root = TempDir::new();
        for name in ["base64", "tr"] {
            let Some(path) = which(name) else {
                eprintln!("skipping: {name} unavailable");
                return;
            };
            symlink(path, root.path().join(name)).unwrap();
        }
        let tmux = root.path().join("tmux");
        fs::write(
            &tmux,
            r#"#!/bin/sh
if [ "$*" = '-N -V' ]; then printf 'tmux 3.2a\n'; exit; fi
shift 4
case "$1" in
    display-message)
        case "$*" in *session_created*|*session_activity*) exit 99 ;; esac
        if [ "$5" = '#{pane_height}' ]; then
            printf '24\n'
        elif [ "$DELUX_TEST_MODE" = missing ]; then
            exit 1
        elif [ "$DELUX_TEST_MODE" = stale-before ] ||
             { [ "$DELUX_TEST_MODE" = stale-after ] && [ -f "$DELUX_TEST_MARKER" ]; }; then
            printf '0\n'
        else
            printf '1\n'
        fi
        ;;
    list-sessions)
        printf '$70 1234 1700000000 1700000001 99\n'
        if [ "$DELUX_TEST_MODE" = bad-activity ]; then
            printf '$7 1234 1700000000 1700000001 +1\n'
        elif [ "$DELUX_TEST_MODE" != vanished-after ]; then
            printf '$7 1234 1700000000 1700000001 1\n'
        fi
        ;;
    list-windows)
        if [ "$DELUX_TEST_MODE" = bad-output ]; then
            printf '1\n-2\n'
        else
            printf '1\n2\n'
        fi
        ;;
    capture-pane)
        [ "$*" = 'capture-pane -p -e -J -S -176 -t $7:' ] || exit 99
        : > "$DELUX_TEST_MARKER"
        [ "$DELUX_TEST_MODE" != capture-failed ] || exit 1
        printf 'marker\377\n\n'
        ;;
    *) exit 99 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = root.path().join("captured");
        let script = preview_script(&sample()).unwrap();
        for (mode, code, captured) in [
            ("good", 0, true),
            ("stale-before", SESSION_CHANGED, false),
            ("stale-after", SESSION_CHANGED, true),
            ("vanished-after", SESSION_CHANGED, true),
            ("missing", SESSION_CHANGED, false),
            ("capture-failed", crate::exit_codes::PROBE_FAILED, true),
            ("bad-activity", crate::exit_codes::PROBE_FAILED, true),
            ("bad-output", crate::exit_codes::PROBE_FAILED, true),
        ] {
            if marker.exists() {
                fs::remove_file(&marker).unwrap();
            }
            let env = environment(&[
                ("PATH", root.path().as_os_str()),
                ("DELUX_TEST_MODE", OsStr::new(mode)),
                ("DELUX_TEST_MARKER", marker.as_os_str()),
            ]);
            let result = run(&env, "/bin/sh", &["-c", &script]);
            assert_eq!(result.status, code, "{mode}: {result:?}");
            assert_eq!(marker.exists(), captured, "{mode}: {result:?}");
            if code == 0 {
                let preview = parse_preview(&result.stdout).unwrap();
                assert_eq!(preview.text, "marker\u{fffd}");
                assert_eq!((preview.activity, preview.output), (1, 2));
            } else if code == SESSION_CHANGED {
                assert_eq!(result.stdout, "DELUX_STALE\n", "{mode}: {result:?}");
            } else {
                assert!(parse_preview(&result.stdout).is_err(), "{result:?}");
            }
        }
    }

    struct Fixture {
        env: Environment,
        binary: PathBuf,
        root: PathBuf,
        _temp: TempDir,
    }

    impl Fixture {
        fn new() -> Option<Self> {
            let Some(binary) = which("tmux") else {
                eprintln!("skipping: tmux integration requires tmux");
                return None;
            };
            let temp = TempDir::new();
            let root = temp.path().join("#{pid},}'\t\n雪");
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let env = environment(&[
                ("TMUX_TMPDIR", root.as_os_str()),
                ("TERM", OsStr::new("xterm")),
                ("LC_ALL", OsStr::new("C")),
            ]);
            // Drop is installed before startup, including assertion failures.
            let fixture = Self {
                env,
                binary,
                root,
                _temp: temp,
            };
            fixture.must_tmux(&[
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "first",
                "printf 'DELUX_PREVIEW_MARKER\\n'; exec sleep 120",
            ]);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let command = fixture.tmux(&[
                    "display-message",
                    "-p",
                    "-t",
                    "first:",
                    "#{pane_current_command}",
                ]);
                assert_eq!(command.status, 0, "{command:?}");
                if command.stdout.trim() == "sleep" {
                    break;
                }
                assert!(Instant::now() < deadline, "pane did not start: {command:?}");
                thread::sleep(Duration::from_millis(5));
            }
            Some(fixture)
        }

        fn tmux(&self, args: &[&str]) -> RunResult {
            let mut arguments = vec!["-u", "-L", "default"];
            arguments.extend_from_slice(args);
            run(&self.env, &self.binary, &arguments)
        }

        fn must_tmux(&self, args: &[&str]) {
            let result = self.tmux(args);
            assert_eq!(result.status, 0, "{args:?}: {result:?}");
        }

        fn script(&self, script: &str) -> RunResult {
            run(&self.env, "/bin/sh", &["-c", script])
        }

        fn script_in_pane(&self, session: &Session, script: &str) -> RunResult {
            let root = self._temp.path();
            let script_path = root.join("pane-script");
            let stdout = root.join("pane-stdout");
            let stderr = root.join("pane-stderr");
            let status_path = root.join("pane-status");
            if status_path.exists() {
                fs::remove_file(&status_path).unwrap();
            }
            fs::write(&script_path, script).unwrap();
            let command = format!(
                "/bin/sh {} > {} 2> {}; printf '%s\\n' \"$?\" > {}; exec sleep 120",
                shell_quote(script_path.to_str().unwrap()),
                shell_quote(stdout.to_str().unwrap()),
                shell_quote(stderr.to_str().unwrap()),
                shell_quote(status_path.to_str().unwrap()),
            );
            // Only respawn this fixture's isolated pane. tmux supplies its real
            // tty, TMUX and TMUX_PANE, just as when Delux runs inside that pane.
            self.must_tmux(&[
                "respawn-pane",
                "-k",
                "-t",
                &format!("{}:", session.id),
                &command,
            ]);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(status) = fs::read_to_string(&status_path)
                    && let Ok(status) = status.trim().parse()
                {
                    return RunResult {
                        stdout: fs::read_to_string(&stdout).unwrap(),
                        stderr: fs::read_to_string(&stderr).unwrap(),
                        status,
                    };
                }
                assert!(Instant::now() < deadline, "pane script timed out");
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn sessions(&self) -> Vec<Session> {
            let result = self.script(INVENTORY_SCRIPT);
            assert_eq!(result.status, 0, "{result:?}");
            parse_inventory(&result.stdout).unwrap()
        }

        fn session(&self) -> Session {
            let mut sessions = self.sessions();
            assert_eq!(sessions.len(), 1, "{sessions:?}");
            sessions.pop().unwrap()
        }

        fn wait_attached(&self, expected: i64) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let observed = self.session().attached;
                if observed == expected {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "expected {expected} clients, got {observed}"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }

        fn start_client(&self, session: &Session) -> Client {
            let (master, slave) = open_pty(24, 80);
            let mut tty = [0 as libc::c_char; 1024];
            // SAFETY: slave is a valid fd and tty is writable for the given length.
            assert_eq!(
                unsafe { libc::ttyname_r(slave.as_raw_fd(), tty.as_mut_ptr(), tty.len()) },
                0
            );
            // SAFETY: successful ttyname_r writes a NUL-terminated string.
            let tty = unsafe { std::ffi::CStr::from_ptr(tty.as_ptr()) }
                .to_str()
                .unwrap()
                .to_owned();
            // SAFETY: master is valid; fcntl updates only its descriptor flags.
            unsafe {
                let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
                assert!(flags >= 0);
                assert_eq!(
                    libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
                    0
                );
            }
            let slave = File::from(slave);
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", &attach_script(session, true).unwrap()])
                .env_clear()
                .envs(&self.env)
                .stdin(slave.try_clone().unwrap())
                .stdout(slave.try_clone().unwrap())
                .stderr(slave);
            // SAFETY: these async-signal-safe calls only configure the child.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = command.spawn().unwrap();
            let pid = child.id();
            let (cancel, receiver) = mpsc::channel();
            let watchdog = thread::spawn(move || {
                if receiver.recv_timeout(Duration::from_secs(5))
                    == Err(mpsc::RecvTimeoutError::Timeout)
                {
                    kill_group(pid);
                }
            });
            let stopped = Arc::new(AtomicBool::new(false));
            let done = stopped.clone();
            let reader = thread::spawn(move || {
                let mut master = File::from(master);
                let mut buffer = [0; 8192];
                while !done.load(Ordering::Relaxed) {
                    match master.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(_) => (),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => (),
                        Err(_) => break,
                    }
                }
            });
            Client {
                child,
                tty,
                status: None,
                cancel,
                watchdog: Some(watchdog),
                stopped,
                reader: Some(reader),
            }
        }

        fn detach(&self, client: &mut Client) {
            self.must_tmux(&["detach-client", "-t", &client.tty]);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = client.poll() {
                    assert!(status.success(), "detach: {status}");
                    return;
                }
                assert!(Instant::now() < deadline, "detach timed out");
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.tmux(&["-N", "kill-server"]);
        }
    }

    struct Client {
        child: Child,
        tty: String,
        status: Option<ExitStatus>,
        cancel: mpsc::Sender<()>,
        watchdog: Option<thread::JoinHandle<()>>,
        stopped: Arc<AtomicBool>,
        reader: Option<thread::JoinHandle<()>>,
    }

    impl Client {
        fn poll(&mut self) -> Option<ExitStatus> {
            if self.status.is_none() {
                self.status = self.child.try_wait().unwrap();
                if self.status.is_some() {
                    let _ = self.cancel.send(());
                }
            }
            self.status
        }
    }

    impl Drop for Client {
        fn drop(&mut self) {
            let _ = self.cancel.send(());
            if let Some(watchdog) = self.watchdog.take() {
                let _ = watchdog.join();
            }
            if self.status.is_none() {
                if self.child.try_wait().ok().flatten().is_none() {
                    kill_group(self.child.id());
                }
                let _ = self.child.wait();
            }
            self.stopped.store(true, Ordering::Relaxed);
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    #[test]
    fn live_switch_preserves_client_and_rejects_stale_identity() {
        let _env = env_read();
        let Some(f) = Fixture::new() else {
            return;
        };
        f.must_tmux(&["rename-session", "-t", "first", "A"]);
        let source = f.session();
        let mut client = f.start_client(&source);
        f.wait_attached(1);
        f.must_tmux(&["new-session", "-d", "-s", "B", "sleep 120"]);
        let target = f.sessions().into_iter().find(|s| s.name == "B").unwrap();
        let client_session = || {
            let result = f.tmux(&["list-clients", "-F", "#{session_name}"]);
            assert_eq!(result.status, 0, "{result:?}");
            result.stdout
        };
        assert_eq!(client_session(), "A\n");
        for mutate in [
            (|s: &mut Session| s.server_pid += 1) as fn(&mut Session),
            |s| s.server_start += 1,
            |s| s.socket_path.push_str("other"),
        ] {
            let mut stale = target.clone();
            mutate(&mut stale);
            let result = f.script_in_pane(&source, &switch_script(&stale).unwrap());
            assert_eq!(result.status, SESSION_CHANGED, "{stale:?}: {result:?}");
            assert!(result.stdout.contains("DELUX_STALE"), "{result:?}");
            assert_eq!(client_session(), "A\n", "{stale:?}: {result:?}");
        }
        let result = f.script_in_pane(&source, &switch_script(&target).unwrap());
        assert_eq!(result.status, 0, "{result:?}");
        assert_eq!(client_session(), "B\n");
        assert!(
            client.poll().is_none(),
            "switch terminated the existing client"
        );
        let sessions = f.sessions();
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions
                .iter()
                .find(|s| s.id == source.id)
                .unwrap()
                .attached,
            0
        );
        assert_eq!(
            sessions
                .iter()
                .find(|s| s.id == target.id)
                .unwrap()
                .attached,
            1
        );
        f.detach(&mut client);
    }

    #[test]
    fn live_preview_tracks_output_from_every_window() {
        let _env = env_read();
        let Some(f) = Fixture::new() else {
            return;
        };
        let session = f.session();
        let script = preview_script(&session).unwrap();
        let first = f.script(&script);
        assert_eq!(first.status, 0, "{first:?}");
        let first = parse_preview(&first.stdout).unwrap();
        // Keep the selected window quiet: output in another window must count.
        f.must_tmux(&[
            "new-window",
            "-d",
            "-t",
            &session.id,
            "-n",
            "printing",
            "while :; do printf 'live\\n'; sleep 1; done",
        ]);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let result = f.script(&script);
            assert_eq!(result.status, 0, "{result:?}");
            let preview = parse_preview(&result.stdout).unwrap();
            assert!(preview.activity >= session.created);
            assert!(preview.text.contains("DELUX_PREVIEW_MARKER"));
            if preview.output > first.output {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "output did not advance: {preview:?}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn creation_rejects_duplicate_and_invalid_directory_without_mutation() {
        let _env = env_read();
        let Some(f) = Fixture::new() else {
            return;
        };
        let original = f.session();
        let marker = f._temp.path().join("unexpected-command");
        let mut request = NewSession {
            name: original.name.clone(),
            directory: f._temp.path().to_str().unwrap().into(),
            command: format!(
                "touch {}; exec sleep 120",
                shell_quote(marker.to_str().unwrap())
            ),
        };
        let duplicate = f.script(&create_script(&request).unwrap());
        assert_ne!(duplicate.status, 0, "{duplicate:?}");
        assert!(!marker.exists());
        assert_eq!(f.session(), original);

        request.name = "invalid-directory".into();
        request.directory = f
            ._temp
            .path()
            .join("does-not-exist")
            .to_str()
            .unwrap()
            .into();
        let invalid = f.script(&create_script(&request).unwrap());
        assert_ne!(invalid.status, 0, "{invalid:?}");
        assert!(!marker.exists());
        assert_eq!(f.session(), original);
        f.must_tmux(&["kill-server"]);
        let invalid = f.script(&create_script(&request).unwrap());
        assert_ne!(invalid.status, 0, "{invalid:?}");
        assert!(
            f.sessions().is_empty(),
            "invalid directory started a server"
        );
    }

    #[test]
    fn creation_preserves_literal_metadata_and_runs_the_authored_command_once() {
        let _env = env_read();
        let Some(f) = Fixture::new() else {
            return;
        };
        let original = f.session();
        let literal = "literal #{pid} #(exit 99) $(exit 99) ' ;";
        let directory = f._temp.path().join(literal);
        fs::create_dir(&directory).unwrap();
        let marker = f._temp.path().join("command-output");
        let request = NewSession {
            name: literal.into(),
            directory: directory.to_str().unwrap().into(),
            command: format!(
                "printf '%s' {} >> {}; exec sleep 120;",
                shell_quote(literal),
                shell_quote(marker.to_str().unwrap()),
            ),
        };
        let result = f.script(&create_script(&request).unwrap());
        assert_eq!(result.status, 0, "{result:?}");
        let (created, inventory) = parse_created_session(&result.stdout).unwrap();
        assert_eq!(inventory.len(), 2);
        assert!(inventory.iter().any(|session| session == &original));
        assert_eq!(created.name, literal);
        assert_eq!(
            fs::canonicalize(&created.path).unwrap(),
            fs::canonicalize(&directory).unwrap()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read_to_string(&marker).unwrap_or_default() != literal {
            assert!(
                Instant::now() < deadline,
                "authored command did not run once"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(fs::read_to_string(&marker).unwrap(), literal);
        assert_eq!(created.socket_path, original.socket_path);
        assert_eq!(created.server_pid, original.server_pid);
        assert_ne!(created.id, original.id);

        // A same-name replacement must never satisfy the captured creation.
        f.must_tmux(&["kill-session", "-t", &created.id]);
        let replacement = f.script(&create_script(&request).unwrap());
        assert_eq!(replacement.status, 0, "{replacement:?}");
        let (replacement, _) = parse_created_session(&replacement.stdout).unwrap();
        assert_ne!(created.id, replacement.id);
        let (_, rest) = result.stdout.split_once('\n').unwrap();
        let (identity, _) = rest.split_once('\n').unwrap();
        let raced = format!("DELUX_CREATED_1\n{identity}\n{}", frame(&f.sessions()));
        assert!(parse_created_session(&raced).is_err());
        assert_eq!(
            f.script(&preview_script(&created).unwrap()).status,
            SESSION_CHANGED
        );
    }

    #[test]
    fn explicit_creation_starts_missing_server_and_resolves_target_home() {
        let _env = env_read();
        let Some(mut f) = Fixture::new() else {
            return;
        };
        f.must_tmux(&["kill-server"]);
        assert!(f.sessions().is_empty());
        // A newly started server must not read the developer's tmux config.
        f.env
            .insert("HOME".into(), f._temp.path().as_os_str().into());
        f.env
            .insert("XDG_CONFIG_HOME".into(), f._temp.path().as_os_str().into());
        f.env.insert("LC_ALL".into(), "POSIX".into());
        let subdirectory = f._temp.path().join("literal #{pid}");
        fs::create_dir(&subdirectory).unwrap();
        for (index, directory) in ["", "~", "~/literal #{pid}"].into_iter().enumerate() {
            let request = NewSession {
                name: format!("created-{index}"),
                directory: directory.into(),
                command: String::new(),
            };
            let result = f.script(&create_script(&request).unwrap());
            assert_eq!(result.status, 0, "{result:?}");
            let (created, inventory) = parse_created_session(&result.stdout).unwrap();
            assert_eq!(inventory.len(), index + 1);
            assert_eq!(created.name, request.name);
            let expected = if index == 2 {
                &subdirectory
            } else {
                f._temp.path()
            };
            assert_eq!(
                fs::canonicalize(&created.path).unwrap(),
                fs::canonicalize(expected).unwrap()
            );
            let observed = f
                .sessions()
                .into_iter()
                .find(|s| s.id == created.id)
                .unwrap();
            assert_eq!(created.server_pid, observed.server_pid);
            assert_eq!(created.server_start, observed.server_start);
            assert_eq!(created.created, observed.created);
            assert_eq!(created.socket_path, observed.socket_path);
            let locale = f.tmux(&["show-environment", "-g", "LC_ALL"]);
            assert_eq!(locale.status, 0, "{locale:?}");
            assert_eq!(locale.stdout.trim(), "LC_ALL=POSIX");
        }
    }

    #[test]
    fn live_sessions_support_preview_attachment_and_identity_guards() {
        let _env = env_read();
        let Some(f) = Fixture::new() else {
            return;
        };
        let original = f.session();
        assert_eq!(original.command, "sleep");
        assert!(!original.path.is_empty());
        assert!(original.activity >= original.created);
        let preview = f.script(&preview_script(&original).unwrap());
        assert_eq!(preview.status, 0, "{preview:?}");
        assert!(
            parse_preview(&preview.stdout)
                .unwrap()
                .text
                .contains("DELUX_PREVIEW_MARKER"),
            "{preview:?}"
        );
        // SAFETY: getuid has no pointer arguments or preconditions.
        let uid = unsafe { libc::getuid() };
        let socket = fs::canonicalize(&f.root)
            .unwrap()
            .join(format!("tmux-{uid}/default"));
        assert_eq!(Path::new(&original.socket_path), socket);
        let name = "雪 ' #{pid},} $(false)";
        f.must_tmux(&[
            "rename-session",
            "-t",
            &original.id,
            &name.replace('#', "##"),
        ]);
        let renamed = f.session();
        assert_eq!(renamed.name, name);
        let mut first = f.start_client(&original);
        f.wait_attached(1);
        let mut second = f.start_client(&original);
        f.wait_attached(2);
        assert!(first.poll().is_none(), "second attachment detached first");
        f.detach(&mut second);
        f.wait_attached(1);
        assert!(first.poll().is_none(), "first client exited");
        f.detach(&mut first);
        let detached = f.session();
        assert!(detached.activity >= renamed.activity);
        let mut expected = renamed;
        expected.activity = detached.activity;
        assert_eq!(detached, expected);
        let renamed = detached;

        for mutate in [
            (|s: &mut Session| s.server_pid += 1) as fn(&mut Session),
            |s| s.server_start += 1,
            |s| s.socket_path.push_str("other"),
        ] {
            let mut stale = renamed.clone();
            mutate(&mut stale);
            let result = f.script(&attach_script(&stale, false).unwrap());
            assert_eq!(result.status, SESSION_CHANGED, "{stale:?}: {result:?}");
            assert!(result.stdout.contains("DELUX_STALE"), "{result:?}");
            let result = f.script(&preview_script(&stale).unwrap());
            assert_eq!(result.status, SESSION_CHANGED, "{stale:?}: {result:?}");
            assert!(result.stdout.contains("DELUX_STALE"), "{result:?}");
        }
        assert_eq!(f.session(), renamed);
        let direct = f.tmux(&["attach-session", "-t", &renamed.id]);
        let guarded = f.script(&attach_script(&renamed, false).unwrap());
        assert_ne!(direct.status, 0, "{direct:?}");
        assert_eq!(guarded.status, direct.status, "{guarded:?}, {direct:?}");
        f.must_tmux(&["new-session", "-d", "-s", "survivor", "sleep 120"]);
        assert_eq!(f.sessions().len(), 2);
        f.must_tmux(&["kill-session", "-t", &original.id]);
        assert_ne!(
            f.script(&attach_script(&original, false).unwrap()).status,
            0
        );
        let preview = f.script(&preview_script(&original).unwrap());
        assert_eq!(preview.status, SESSION_CHANGED, "{preview:?}");
        assert!(preview.stdout.contains("DELUX_STALE"), "{preview:?}");
        let remaining = f.session();
        assert_eq!(remaining.name, "survivor");
        assert_ne!(remaining.id, original.id);
        assert_eq!(remaining.attached, 0);
        f.must_tmux(&["kill-server"]);
        f.must_tmux(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "replacement",
            "sleep 120",
        ]);
        let replacement = f.session();
        assert_eq!(replacement.id, original.id);
        assert_ne!(replacement.server_pid, original.server_pid);
        assert_eq!(
            f.script(&attach_script(&original, false).unwrap()).status,
            SESSION_CHANGED
        );
        let preview = f.script(&preview_script(&original).unwrap());
        assert_eq!(preview.status, SESSION_CHANGED, "{preview:?}");
        assert!(preview.stdout.contains("DELUX_STALE"), "{preview:?}");
        assert_eq!(f.session(), replacement);
        f.must_tmux(&["kill-server"]);
        assert!(f.sessions().is_empty());
        assert_ne!(
            f.script(&attach_script(&replacement, false).unwrap())
                .status,
            0
        );
        let preview = f.script(&preview_script(&replacement).unwrap());
        assert_eq!(preview.status, SESSION_CHANGED, "{preview:?}");
        assert!(preview.stdout.contains("DELUX_STALE"), "{preview:?}");
        assert!(
            f.sessions().is_empty(),
            "attach or preview restarted server"
        );
    }

    #[test]
    fn live_agent_status_covers_multiple_windows_panes_and_sessions() {
        let _env = env_read();
        let Some(f) = Fixture::new() else { return };
        f.must_tmux(&["rename-window", "-t", "first:", "one"]);
        f.must_tmux(&[
            "set-option",
            "-p",
            "-t",
            "first:one",
            "@agent_status",
            "done 90",
        ]);
        f.must_tmux(&["split-window", "-d", "-t", "first:one", "sleep 120"]);
        f.must_tmux(&[
            "set-option",
            "-p",
            "-t",
            "first:one.1",
            "@agent_status",
            "working 50",
        ]);
        f.must_tmux(&["new-window", "-d", "-t", "first:", "-n", "two", "sleep 120"]);
        f.must_tmux(&[
            "set-option",
            "-p",
            "-t",
            "first:two",
            "@agent_status",
            "waiting 10",
        ]);
        f.must_tmux(&["new-session", "-d", "-s", "second", "sleep 120"]);
        f.must_tmux(&[
            "set-option",
            "-p",
            "-t",
            "second:",
            "@agent_status",
            "done 30",
        ]);
        f.must_tmux(&["new-session", "-d", "-s", "third", "sleep 120"]);
        f.must_tmux(&[
            "set-option",
            "-p",
            "-t",
            "third:",
            "@agent_status",
            "not-a-state",
        ]);
        let sessions = f.sessions();
        assert_eq!(sessions.len(), 3);
        let result = f.script(STATUS_SCRIPT);
        assert_eq!(result.status, 0, "{result:?}");
        let rows = parse_status(&result.stdout).unwrap();
        assert_eq!(rows.len(), 3);
        for session in sessions {
            let expected = match session.name.as_str() {
                "first" => parse_agent_status("waiting 10"),
                "second" => parse_agent_status("done 30"),
                "third" => None,
                name => panic!("unexpected session {name}"),
            };
            assert_eq!(session.agent, expected);
            assert!(rows.contains(&AgentStatusRow {
                server_pid: session.server_pid,
                server_start: session.server_start,
                session_id: session.id,
                output: session.output,
                agent: expected,
            }));
        }
        f.must_tmux(&["kill-server"]);
        let result = f.script(STATUS_SCRIPT);
        assert_eq!(result.status, 0, "{result:?}");
        assert!(parse_status(&result.stdout).unwrap().is_empty());
    }

    #[test]
    fn live_agent_status_rejects_record_injection_before_framing() {
        let _env = env_read();
        let Some(f) = Fixture::new() else { return };
        let session = f.session();
        for value in [
            "invalid\nwaiting 99".to_owned(),
            format!(
                "done\n{} {} {} 1 waiting 99",
                session.server_pid, session.server_start, session.id
            ),
            "working\nDELUX_END\nDELUX_STATUS_2".to_owned(),
            "waiting\t1".to_owned(),
            "waiting\r".to_owned(),
            "#{pane_program_state}".to_owned(),
        ] {
            f.must_tmux(&["set-option", "-p", "-t", "first:", "@agent_status", &value]);
            assert_eq!(f.session().agent, None, "{value:?}");
            let result = f.script(STATUS_SCRIPT);
            assert_eq!(result.status, 0, "{result:?}");
            let rows = parse_status(&result.stdout).unwrap();
            assert_eq!(rows.len(), 1, "{value:?}");
            assert_eq!(rows[0].agent, None, "{value:?}");
        }
    }

    #[test]
    fn live_native_osc_status_takes_precedence_over_fallback() {
        let _env = env_read();
        let Some(f) = Fixture::new() else { return };
        let session = f.session();
        for (native, expected) in [
            ("working", "working"),
            ("blocked", "waiting"),
            ("error", "error"),
            ("done", "done"),
            ("idle", ""),
            ("clear", "waiting 7"),
        ] {
            f.must_tmux(&[
                "set-option",
                "-p",
                "-t",
                "first:",
                "@agent_status",
                "waiting 7",
            ]);
            let emitted = f.script_in_pane(
                &session,
                &format!("printf '\\033]7501;state={native}\\007' > /dev/tty"),
            );
            assert_eq!(emitted.status, 0, "{emitted:?}");
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let value = f.tmux(&[
                    "display-message",
                    "-p",
                    "-t",
                    "first:",
                    "#{pane_program_state}",
                ]);
                assert_eq!(value.status, 0, "{value:?}");
                if value.stdout.trim() == if native == "clear" { "" } else { native } {
                    break;
                }
                if native == "working"
                    && Instant::now() >= deadline
                    && value.stdout.trim().is_empty()
                {
                    eprintln!("skipping: tmux does not expose OSC 7501 program status");
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "OSC state not received: {native}"
                );
                thread::sleep(Duration::from_millis(5));
            }
            let expected = parse_agent_status(expected);
            assert_eq!(f.session().agent, expected, "{native}");
            let result = f.script(STATUS_SCRIPT);
            assert_eq!(result.status, 0, "{result:?}");
            let rows = parse_status(&result.stdout).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].agent, expected, "{native}");
        }
    }

    #[test]
    fn inventory_distinguishes_absent_server_from_broken_socket() {
        let _env = env_read();
        if which("tmux").is_none() {
            eprintln!("skipping: tmux integration requires tmux");
            return;
        }
        let root = TempDir::new();
        let env = environment(&[
            ("TMUX_TMPDIR", root.path().as_os_str()),
            ("LC_ALL", OsStr::new("C")),
            ("TMUX", OsStr::new("unrelated,999,1")),
        ]);
        let result = run(&env, "/bin/sh", &["-c", INVENTORY_SCRIPT]);
        assert_eq!(result.status, 0, "{result:?}");
        assert!(parse_inventory(&result.stdout).unwrap().is_empty());
        // SAFETY: getuid has no pointer arguments or preconditions.
        let uid = unsafe { libc::getuid() };
        let directory = root.path().join(format!("tmux-{uid}"));
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let _listener = UnixDatagram::bind(directory.join("default")).unwrap();
        let result = run(&env, "/bin/sh", &["-c", INVENTORY_SCRIPT]);
        assert_eq!(result.status, crate::exit_codes::PROBE_FAILED, "{result:?}");
        assert!(parse_inventory(&result.stdout).is_err(), "{result:?}");
    }
}
