use std::path::Path;

use super::json::Json;

use super::{Agent, Integration, storage};

pub(super) struct Entry<'a> {
    pub event: &'a str,
    pub matcher: Option<&'a str>,
    pub command: String,
}

pub(super) trait HookAgent {
    fn entries(&self, executable: &str) -> Vec<Entry<'_>>;
    fn owns(&self, command: &str) -> bool;
    fn add_handler_fields(&self, fields: &mut Vec<(String, Json)>);
}

impl HookAgent for Agent {
    fn entries(&self, executable: &str) -> Vec<Entry<'_>> {
        let Integration::Hooks { events, .. } = self.integration else {
            unreachable!()
        };
        events
            .iter()
            .map(|event| Entry {
                event: event.name,
                matcher: event.matcher,
                command: format!(
                    "{executable} agent-status hook {} {}",
                    self.name, event.name
                ),
            })
            .collect()
    }

    fn owns(&self, command: &str) -> bool {
        command
            .rsplit_once(" agent-status hook ")
            .is_some_and(|(executable, suffix)| {
                super::paths::is_handler_executable(executable)
                    && suffix.split_once(' ').is_some_and(|(name, event)| {
                        name == self.name
                            && !event.is_empty()
                            && event
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    })
            })
    }

    fn add_handler_fields(&self, fields: &mut Vec<(String, Json)>) {
        let Integration::Hooks { timeout, .. } = self.integration else {
            unreachable!()
        };
        fields.push(("timeout".into(), Json::Number(timeout.into())));
    }
}

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

fn owned(handler: &Json, owns: &impl Fn(&str) -> bool) -> bool {
    handler
        .get("command")
        .and_then(Json::as_str)
        .is_some_and(owns)
}

fn group(entry: Entry<'_>, add_fields: &impl Fn(&mut Vec<(String, Json)>)) -> Json {
    let mut fields = Vec::new();
    if let Some(matcher) = entry.matcher {
        fields.push(("matcher".into(), Json::String(matcher.into())));
    }
    let mut handler = vec![
        ("type".into(), Json::String("command".into())),
        ("command".into(), Json::String(entry.command)),
    ];
    add_fields(&mut handler);
    fields.push(("hooks".into(), Json::Array(vec![Json::Object(handler)])));
    Json::Object(fields)
}

struct ExpectedGroup<'a> {
    event: &'a str,
    value: Json,
}

fn expected<'a>(agent: &'a impl HookAgent, command: &str) -> Vec<ExpectedGroup<'a>> {
    agent
        .entries(command)
        .into_iter()
        .map(|entry| ExpectedGroup {
            event: entry.event,
            value: group(entry, &|fields| agent.add_handler_fields(fields)),
        })
        .collect()
}

fn reporting(
    value: &Json,
    expected: &[ExpectedGroup<'_>],
    owns: &impl Fn(&str) -> bool,
) -> &'static str {
    let Some(hooks) = value.get("hooks").and_then(Json::as_object) else {
        return "not installed";
    };
    let count: usize = effective_entries(hooks)
        .map(|groups| {
            groups
                .as_array()
                .expect("validated event")
                .iter()
                .map(|group| {
                    group
                        .get("hooks")
                        .and_then(Json::as_array)
                        .expect("validated group")
                        .iter()
                        .filter(|handler| owned(handler, owns))
                        .count()
                })
                .sum::<usize>()
        })
        .sum();
    if count == 0 {
        return "not installed";
    }
    // Other agents can append groups after ours. Their position must not make
    // a complete installation look partial or trigger endless reinstall churn.
    if count == expected.len()
        && expected.iter().all(|entry| {
            value
                .get("hooks")
                .and_then(|hooks| hooks.get(entry.event))
                .and_then(Json::as_array)
                .is_some_and(|groups| groups.contains(&entry.value))
        })
    {
        "installed"
    } else {
        "partly installed"
    }
}

fn edited(mut value: Json, entries: Vec<ExpectedGroup<'_>>, owns: &impl Fn(&str) -> bool) -> Json {
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
                handlers.retain(|handler| !owned(handler, owns));
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
    if !entries.is_empty() {
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
        for entry in entries {
            let position = hooks
                .iter()
                .rposition(|(name, _)| name == entry.event)
                .unwrap_or_else(|| {
                    hooks.push((entry.event.into(), Json::Array(Vec::new())));
                    hooks.len() - 1
                });
            hooks[position]
                .1
                .as_array_mut()
                .expect("validated event")
                .push(entry.value);
        }
    }
    value
}

pub(super) fn update(
    agent: &impl HookAgent,
    path: &Path,
    command: &str,
    install: bool,
) -> Result<storage::Update, String> {
    storage::update(path, true, |raw| {
        if raw.is_none() && !install {
            return Ok(None);
        }
        let original = document(raw, path)?;
        let entries = if install {
            expected(agent, command)
        } else {
            Vec::new()
        };
        if install && reporting(&original, &entries, &|command| agent.owns(command)) == "installed"
        {
            return Ok(raw.map(str::to_owned));
        }
        let replacement = edited(original.clone(), entries, &|command| agent.owns(command));
        if original == replacement {
            return Ok(raw.map(str::to_owned));
        }
        let mut raw =
            serde_json::to_string_pretty(&replacement).map_err(|error| error.to_string())?;
        raw.push('\n');
        Ok(Some(raw))
    })
}

pub(super) fn status(
    agent: &impl HookAgent,
    path: &Path,
    command: &str,
) -> Result<&'static str, String> {
    let raw = storage::read(path)?;
    let original = document(raw.as_deref(), path)?;
    Ok(reporting(
        &original,
        &expected(agent, command),
        &|command| agent.owns(command),
    ))
}
