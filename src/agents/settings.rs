use std::path::Path;

use super::json::Json;

use super::{Agent, Event, Integration, storage};

fn effective_entries(entries: &[(String, Json)]) -> impl Iterator<Item = &Json> {
    entries
        .iter()
        .enumerate()
        .filter(|(index, (name, _))| !entries[index + 1..].iter().any(|(later, _)| later == name))
        .map(|(_, (_, value))| value)
}

fn document(raw: Option<&str>, path: &Path) -> Result<Json, String> {
    let value = match raw {
        Some(raw) => serde_json::from_str(raw)
            .map_err(|error| format!("{}: invalid JSON: {error}", path.display()))?,
        None => Json::Object(vec![("hooks".into(), Json::Object(Vec::new()))]),
    };
    let invalid = || {
        format!(
            "{}: expected an object containing hooks objects, event arrays and groups with handler arrays; nothing was changed",
            path.display()
        )
    };
    value.as_object().ok_or_else(invalid)?;
    if let Some(hooks) = value.get("hooks") {
        for groups in effective_entries(hooks.as_object().ok_or_else(invalid)?) {
            for group in groups.as_array().ok_or_else(invalid)? {
                group.as_object().ok_or_else(invalid)?;
                let handlers = group
                    .get("hooks")
                    .and_then(Json::as_array)
                    .ok_or_else(invalid)?;
                if handlers.iter().any(|handler| handler.as_object().is_none()) {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(value)
}

fn owned(handler: &Json, agent: &Agent) -> bool {
    let Some(command) = handler.get("command").and_then(Json::as_str) else {
        return false;
    };
    command
        .rsplit_once(" agent-status hook ")
        .is_some_and(|(executable, suffix)| {
            !executable.is_empty()
                && suffix.split_once(' ').is_some_and(|(name, event)| {
                    name == agent.name
                        && !event.is_empty()
                        && !event.chars().any(char::is_whitespace)
                })
        })
}

fn group(agent: &Agent, event: &Event, command: &str) -> Json {
    let mut fields = Vec::new();
    if let Some(matcher) = event.matcher {
        fields.push(("matcher".into(), Json::String(matcher.into())));
    }
    fields.push((
        "hooks".into(),
        Json::Array(vec![Json::Object(vec![
            ("type".into(), Json::String("command".into())),
            (
                "command".into(),
                Json::String(format!(
                    "{command} agent-status hook {} {}",
                    agent.name, event.name
                )),
            ),
            ("timeout".into(), Json::Number(5.into())),
        ])]),
    ));
    Json::Object(fields)
}

fn edited(mut value: Json, agent: &Agent, command: &str, install: bool) -> Json {
    if let Some(hooks) = value.get_mut("hooks") {
        let hooks = hooks.as_object_mut().expect("validated hooks");
        let mut index = 0;
        while index < hooks.len() {
            if hooks[index + 1..]
                .iter()
                .any(|(name, _)| name == &hooks[index].0)
            {
                index += 1;
                continue;
            }
            let groups = hooks[index].1.as_array_mut().expect("validated event");
            let before = groups.len();
            groups.retain_mut(|group| {
                let handlers = group
                    .get_mut("hooks")
                    .and_then(Json::as_array_mut)
                    .expect("validated group");
                let before = handlers.len();
                handlers.retain(|handler| !owned(handler, agent));
                // Empty groups belonging to someone else are not ours to remove.
                before == handlers.len() || !handlers.is_empty()
            });
            let emptied = before != groups.len() && groups.is_empty();
            // Keep the last empty entry when removing it would expose a
            // shadowed duplicate; that earlier value belongs to the user.
            if emptied
                && !hooks[..index]
                    .iter()
                    .any(|(name, _)| name == &hooks[index].0)
            {
                hooks.remove(index);
            } else {
                index += 1;
            }
        }
    }
    if install {
        let Integration::Hooks { events, .. } = agent.integration else {
            unreachable!()
        };
        if value.get("hooks").is_none() {
            value
                .as_object_mut()
                .expect("validated object")
                .push(("hooks".into(), Json::Object(Vec::new())));
        }
        let hooks = value
            .get_mut("hooks")
            .and_then(Json::as_object_mut)
            .expect("validated hooks");
        for event in events {
            let position = hooks
                .iter()
                .rposition(|(name, _)| name == event.name)
                .unwrap_or_else(|| {
                    hooks.push((event.name.into(), Json::Array(Vec::new())));
                    hooks.len() - 1
                });
            hooks[position]
                .1
                .as_array_mut()
                .expect("validated event")
                .push(group(agent, event, command));
        }
    }
    value
}

pub(super) fn update(
    agent: &Agent,
    path: &Path,
    command: &str,
    install: bool,
) -> Result<storage::Update, String> {
    storage::update(path, true, |raw| {
        if raw.is_none() && !install {
            return Ok(None);
        }
        let original = document(raw, path)?;
        let replacement = edited(original.clone(), agent, command, install);
        if original == replacement {
            return Ok(raw.map(str::to_owned));
        }
        let mut raw =
            serde_json::to_string_pretty(&replacement).map_err(|error| error.to_string())?;
        raw.push('\n');
        Ok(Some(raw))
    })
}

pub(super) fn status(agent: &Agent, path: &Path, command: &str) -> Result<&'static str, String> {
    let raw = storage::read(path)?;
    let original = document(raw.as_deref(), path)?;
    let any = original
        .get("hooks")
        .and_then(Json::as_object)
        .is_some_and(|hooks| {
            effective_entries(hooks).any(|groups| {
                groups
                    .as_array()
                    .expect("validated event")
                    .iter()
                    .any(|group| {
                        group
                            .get("hooks")
                            .and_then(Json::as_array)
                            .expect("validated group")
                            .iter()
                            .any(|handler| owned(handler, agent))
                    })
            })
        });
    Ok(if !any {
        "not installed"
    } else if original == edited(original.clone(), agent, command, true) {
        "installed"
    } else {
        "partly installed"
    })
}
