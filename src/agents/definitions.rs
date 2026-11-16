use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::json::Json;
use super::reporter::Action;
use super::settings::{self, Entry, HookAgent};
use super::{AGENTS, Context, display, paths, valid_name};

const DEFINITION_LIMIT: usize = 64 * 1024;

#[derive(Debug)]
pub(super) struct UserEvent {
    pub name: String,
    pub matcher: Option<String>,
    pub action: Action,
}

#[derive(Debug)]
pub(super) struct UserAgent {
    pub id: String,
    pub name: String,
    pub executable: Option<String>,
    pub file: PathBuf,
    pub events: Vec<UserEvent>,
    pub handler: Vec<(String, Json)>,
}

impl HookAgent for UserAgent {
    fn entries(&self, executable: &str) -> Vec<Entry<'_>> {
        self.events
            .iter()
            .map(|event| {
                let command = match event.action {
                    Action::Set(state) => {
                        format!("{executable} agent-status set {state} --agent {}", self.id)
                    }
                    Action::Clear => format!("{executable} agent-status clear --agent {}", self.id),
                };
                Entry {
                    event: &event.name,
                    matcher: event.matcher.as_deref(),
                    command,
                }
            })
            .collect()
    }

    fn owns(&self, command: &str) -> bool {
        command.contains(" agent-status ")
            && command
                .rsplit_once(" --agent ")
                .is_some_and(|(_, id)| id == self.id)
    }

    fn add_handler_fields(&self, fields: &mut Vec<(String, Json)>) {
        fields.extend(self.handler.iter().cloned());
    }
}

fn state(value: &str) -> Result<Action, String> {
    match value {
        "working" => Ok(Action::Set("working")),
        "waiting" => Ok(Action::Set("waiting")),
        "done" => Ok(Action::Set("done")),
        "clear" => Ok(Action::Clear),
        _ => Err("event state must be working, waiting, done or clear".into()),
    }
}

pub(super) fn parse(id: &str, raw: &str, home: Option<&Path>) -> Result<UserAgent, String> {
    if !valid_name(id) {
        return Err("definition id must match [a-z0-9][a-z0-9-]{0,31}".into());
    }
    if AGENTS.iter().any(|agent| agent.name == id) {
        return Err(format!(
            "definition id {id} conflicts with a built-in agent"
        ));
    }
    if raw.len() > DEFINITION_LIMIT {
        return Err("agent definition exceeds 64 KiB".into());
    }
    let value: Json =
        serde_json::from_str(raw).map_err(|error| format!("invalid JSON: {error}"))?;
    let fields = value
        .as_object()
        .ok_or("agent definition must be a JSON object")?;
    for (key, _) in fields {
        if !matches!(
            key.as_str(),
            "name" | "executable" | "file" | "events" | "handler"
        ) {
            return Err(format!("unknown agent definition field {key}"));
        }
    }
    let name = value
        .get("name")
        .and_then(Json::as_str)
        .ok_or("name must be a string")?;
    if name.is_empty() || name.chars().count() > 64 {
        return Err("name must contain 1–64 characters".into());
    }
    let executable = match value.get("executable") {
        None => None,
        Some(value) => {
            let executable = value
                .as_str()
                .ok_or("executable must be a bare file name")?;
            if executable.is_empty()
                || matches!(executable, "." | "..")
                || executable.contains('/')
                || executable.chars().any(char::is_control)
            {
                return Err("executable must be a bare file name".into());
            }
            Some(executable.to_owned())
        }
    };
    let file = value
        .get("file")
        .and_then(Json::as_str)
        .ok_or("file must be an absolute path or start with ~/")?;
    let file = if let Some(suffix) = file.strip_prefix("~/") {
        home.filter(|home| !home.as_os_str().is_empty())
            .ok_or("$HOME is required to expand ~/")?
            .join(suffix.trim_start_matches('/'))
    } else if Path::new(file).is_absolute() {
        PathBuf::from(file)
    } else {
        return Err("file must be an absolute path or start with ~/".into());
    };
    let events = value
        .get("events")
        .and_then(Json::as_object)
        .ok_or("events must be a non-empty object")?;
    if events.is_empty() || events.len() > 32 {
        return Err("events must contain 1–32 entries".into());
    }
    let mut parsed_events = Vec::with_capacity(events.len());
    for (index, (name, event)) in events.iter().enumerate() {
        if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
            return Err("event names must contain 1–64 characters and no controls".into());
        }
        // As with settings, the final occurrence of a duplicate key is effective.
        if events[index + 1..].iter().any(|(later, _)| later == name) {
            continue;
        }
        let (action, matcher) = match event {
            Json::String(value) => (state(value)?, None),
            Json::Object(fields) => {
                if let Some((key, _)) = fields
                    .iter()
                    .find(|(key, _)| !matches!(key.as_str(), "state" | "matcher"))
                {
                    return Err(format!("unknown event field {key}"));
                }
                let action = state(
                    event
                        .get("state")
                        .and_then(Json::as_str)
                        .ok_or("event state must be a string")?,
                )?;
                let matcher = event
                    .get("matcher")
                    .map(|matcher| {
                        matcher
                            .as_str()
                            .map(str::to_owned)
                            .ok_or("event matcher must be a string")
                    })
                    .transpose()?;
                (action, matcher)
            }
            _ => {
                return Err(
                    "event must be a state string or an object with state and optional matcher"
                        .into(),
                );
            }
        };
        parsed_events.push(UserEvent {
            name: name.clone(),
            matcher,
            action,
        });
    }
    let handler = match value.get("handler") {
        None => Vec::new(),
        Some(handler) => {
            let fields = handler.as_object().ok_or("handler must be an object")?;
            for (key, value) in fields {
                if matches!(key.as_str(), "type" | "command") {
                    return Err(format!("handler may not override {key}"));
                }
                if !matches!(value, Json::String(_) | Json::Number(_) | Json::Bool(_)) {
                    return Err("handler field values must be strings, numbers or booleans".into());
                }
            }
            fields.clone()
        }
    };
    Ok(UserAgent {
        id: id.into(),
        name: name.into(),
        executable,
        file,
        events: parsed_events,
        handler,
    })
}

#[derive(Default)]
pub(super) struct Catalog {
    pub agents: Vec<UserAgent>,
    pub errors: Vec<(PathBuf, String)>,
}

pub(super) fn load(directory: &Path, home: Option<&Path>) -> Catalog {
    let mut catalog = Catalog::default();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return catalog,
        Err(error) => {
            catalog
                .errors
                .push((directory.into(), format!("read definitions: {error}")));
            return catalog;
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) if entry.file_name().as_encoded_bytes().ends_with(b".json") => {
                files.push(entry.path())
            }
            Ok(_) => {}
            Err(error) => catalog
                .errors
                .push((directory.into(), format!("read definition entry: {error}"))),
        }
    }
    files.sort();
    for path in files {
        let parsed = (|| {
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or("definition id is not valid UTF-8")?;
            let mut raw = Vec::new();
            File::open(&path)
                .map_err(|error| format!("open definition: {error}"))?
                .take((DEFINITION_LIMIT + 1) as u64)
                .read_to_end(&mut raw)
                .map_err(|error| format!("read definition: {error}"))?;
            if raw.len() > DEFINITION_LIMIT {
                return Err("agent definition exceeds 64 KiB".into());
            }
            let raw =
                std::str::from_utf8(&raw).map_err(|_| "agent definition is not valid UTF-8")?;
            parse(id, raw, home)
        })();
        match parsed {
            Ok(agent) => catalog.agents.push(agent),
            Err(error) => catalog.errors.push((path, error)),
        }
    }
    catalog
}

#[derive(Default)]
pub(super) struct RunSummary {
    pub selected: usize,
    pub changed: bool,
    pub errors: Vec<String>,
}

pub(super) fn run(
    agents: &[UserAgent],
    context: &Context,
    names: &[String],
    operation: &str,
    command: &str,
    out: &mut dyn Write,
) -> RunSummary {
    let mut summary = RunSummary::default();
    for agent in agents {
        if !names.is_empty() && !names.contains(&agent.id) {
            continue;
        }
        let parent = agent.file.parent().unwrap_or(&agent.file);
        let found = parent.is_dir()
            || agent
                .executable
                .as_deref()
                .is_some_and(|name| paths::executable_on_path(&context.path, name).is_some());
        let result = (|| {
            if operation != "status" && !names.is_empty() && !found {
                let looked = match &agent.executable {
                    Some(name) => format!(
                        "looked for {} on PATH and directory {}",
                        display(name, 100),
                        display(&parent.to_string_lossy(), 800)
                    ),
                    None => format!(
                        "looked for directory {}",
                        display(&parent.to_string_lossy(), 800)
                    ),
                };
                return Err(format!(
                    "{} was not found; {looked}",
                    display(&agent.name, 100)
                ));
            }
            if operation == "install" && !found {
                return Ok(());
            }
            let status = settings::status(agent, &agent.file, command)?;
            if operation == "status" {
                let _ = writeln!(
                    out,
                    "{} (user-defined): {}; reporting {} — {}",
                    display(&agent.name, 64),
                    if found { "found" } else { "not found" },
                    status,
                    display(&agent.file.to_string_lossy(), 800)
                );
                return Ok(());
            }
            if operation == "uninstall" && names.is_empty() && status == "not installed" {
                return Ok(());
            }
            summary.selected += 1;
            let install = operation == "install";
            let update = settings::update(agent, &agent.file, command, install)?;
            summary.changed |= update.changed;
            let message = match (install, update.changed) {
                (true, true) => "installed",
                (true, false) => "already installed",
                (false, true) => "uninstalled",
                (false, false) => "not installed",
            };
            let _ = writeln!(
                out,
                "{} (user-defined): {} — {}",
                display(&agent.name, 64),
                message,
                display(&agent.file.to_string_lossy(), 800)
            );
            if let Some(backup) = update.backup {
                let _ = writeln!(out, "  Backup: {}", display(&backup.to_string_lossy(), 800));
            }
            if install && update.changed {
                let _ = writeln!(
                    out,
                    "  Command: {} agent-status set <state> --agent {} (or clear)",
                    display(command, 800),
                    agent.id
                );
            }
            Ok(())
        })();
        if let Err(error) = result {
            summary.errors.push(error);
        }
    }
    summary
}
