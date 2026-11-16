#[cfg(test)]
mod definition_tests;
mod definitions;
#[cfg(test)]
mod extension_tests;
mod json;
mod paths;
mod reporter;
mod settings;
mod storage;

#[cfg(test)]
mod settings_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::display::display;

#[cfg(test)]
use paths::handler_executable;
use paths::{Context, Directory};
pub(crate) use reporter::report;

const ADAPTER_MARKER: &str = "// delux-agent-status adapter. Installed by `delux agents install`; reinstalling replaces this file.";

struct Event {
    name: &'static str,
    matcher: Option<&'static str>,
}

const fn event(name: &'static str) -> Event {
    Event {
        name,
        matcher: None,
    }
}

enum Integration {
    Hooks {
        file: &'static str,
        events: &'static [Event],
        timeout: u32,
    },
    Extension {
        file: &'static str,
        adapter: &'static str,
    },
}

struct Agent {
    name: &'static str,
    display: &'static str,
    executable: &'static str,
    config_env: Option<&'static str>,
    directory: Directory,
    detect_fallback_parent: bool,
    integration: Integration,
}

// Detection, paths and registration live together so a new built-in has one entry.
const AGENTS: &[Agent] = &[
    Agent {
        name: "claude",
        display: "Claude Code",
        executable: "claude",
        config_env: Some("CLAUDE_CONFIG_DIR"),
        directory: Directory::Home(".claude"),
        detect_fallback_parent: false,
        integration: Integration::Hooks {
            file: "settings.json",
            timeout: 5,
            events: &[
                event("SessionStart"),
                event("UserPromptSubmit"),
                Event {
                    name: "PreToolUse",
                    matcher: Some("AskUserQuestion|ExitPlanMode"),
                },
                event("PermissionRequest"),
                event("PostToolUse"),
                event("PostToolUseFailure"),
                Event {
                    name: "Notification",
                    matcher: Some(
                        "permission_prompt|elicitation_dialog|elicitation_url_dialog|agent_needs_input",
                    ),
                },
                event("Stop"),
                event("StopFailure"),
                event("SessionEnd"),
            ],
        },
    },
    Agent {
        name: "codex",
        display: "Codex",
        executable: "codex",
        config_env: Some("CODEX_HOME"),
        directory: Directory::Home(".codex"),
        detect_fallback_parent: false,
        integration: Integration::Hooks {
            file: "hooks.json",
            timeout: 5,
            events: &[
                event("SessionStart"),
                event("UserPromptSubmit"),
                event("PermissionRequest"),
                event("PostToolUse"),
                event("Stop"),
                event("Interrupt"),
                event("SessionEnd"),
            ],
        },
    },
    Agent {
        name: "gemini",
        display: "Gemini CLI",
        executable: "gemini",
        config_env: None,
        directory: Directory::Parent {
            variable: "GEMINI_CLI_HOME",
            suffix: ".gemini",
        },
        detect_fallback_parent: false,
        integration: Integration::Hooks {
            file: "settings.json",
            timeout: 5000,
            events: &[
                event("SessionStart"),
                event("BeforeAgent"),
                event("AfterTool"),
                Event {
                    name: "Notification",
                    matcher: Some("ToolPermission"),
                },
                event("AfterAgent"),
                event("SessionEnd"),
            ],
        },
    },
    Agent {
        name: "opencode",
        display: "OpenCode",
        executable: "opencode",
        config_env: Some("OPENCODE_CONFIG_DIR"),
        directory: Directory::Config {
            variable: "XDG_CONFIG_HOME",
            default: ".config",
            suffix: "opencode",
        },
        detect_fallback_parent: false,
        integration: Integration::Extension {
            file: "plugins/delux-agent-status.js",
            adapter: include_str!("../adapters/opencode/delux-agent-status.js"),
        },
    },
    Agent {
        name: "pi",
        display: "Pi",
        executable: "pi",
        config_env: Some("PI_CODING_AGENT_DIR"),
        directory: Directory::Home(".pi/agent"),
        detect_fallback_parent: false,
        integration: Integration::Extension {
            file: "extensions/delux-agent-status.ts",
            adapter: include_str!("../adapters/pi/delux-agent-status.ts"),
        },
    },
    Agent {
        name: "omp",
        display: "OMP",
        executable: "omp",
        config_env: Some("PI_CODING_AGENT_DIR"),
        directory: Directory::HomeVariable {
            variable: "PI_CONFIG_DIR",
            default: ".omp",
            suffix: "agent",
        },
        detect_fallback_parent: true,
        integration: Integration::Extension {
            file: "extensions/delux-agent-status.ts",
            adapter: include_str!("../adapters/pi/delux-agent-status.ts"),
        },
    },
];

pub(crate) fn validate(command: &str, args: &[&str]) -> Result<(), String> {
    if command == "agent-status" {
        return match args {
            ["set", "working" | "waiting" | "blocked" | "done" | "error"] | ["clear"] => Ok(()),
            ["set", "working" | "waiting" | "blocked" | "done" | "error", "--agent", name]
            | ["clear", "--agent", name] if valid_name(name) => Ok(()),
            ["hook", ..] => Ok(()),
            _ => Err("usage: delux agent-status set <working|waiting|blocked|done|error> [--agent NAME] | clear [--agent NAME] | hook <agent> <event>; NAME must match [a-z0-9][a-z0-9-]{0,31}".into()),
        };
    }
    match args {
        [] | ["status"] => Ok(()),
        ["install" | "uninstall", ..] => Ok(()),
        _ => Err("usage: delux agents [status | install [NAME...] | uninstall [NAME...]]".into()),
    }
}

fn valid_name(name: &str) -> bool {
    let valid = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    (1..=32).contains(&name.len())
        && name.as_bytes().first().is_some_and(|byte| valid(*byte))
        && name.bytes().all(|byte| valid(byte) || byte == b'-')
}

pub(crate) fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = Context::environment().and_then(|context| run_in(&context, args, out, err));
    finish(result, err)
}

fn finish(result: Result<i32, String>, err: &mut dyn Write) -> i32 {
    match result {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(err, "delux: {}", display(&error, 2400));
            1
        }
    }
}

fn run_in(
    context: &Context,
    args: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let operation = args.first().map_or("status", String::as_str);
    let command = context.command()?;
    let names = args.get(1..).unwrap_or_default();
    let definitions = definitions::load(&context.definitions_dir, context.home.as_deref());
    for (path, error) in &definitions.errors {
        let _ = writeln!(
            out,
            "{} (invalid user-defined agent): {}",
            display(&path.to_string_lossy(), 800),
            display(error, 800)
        );
    }
    if let Some(name) = names.iter().find(|name| {
        !AGENTS.iter().any(|agent| agent.name == name.as_str())
            && !definitions
                .agents
                .iter()
                .any(|agent| agent.id == name.as_str())
    }) {
        let valid_names: Vec<_> = AGENTS
            .iter()
            .map(|agent| agent.name)
            .chain(definitions.agents.iter().map(|agent| agent.id.as_str()))
            .collect();
        let _ = writeln!(
            err,
            "delux: unknown agent {}; valid names: {}",
            display(name, 200),
            valid_names.join(", ")
        );
        return Ok(2);
    }
    let mut selected = 0;
    let mut changed_any = false;
    let mut errors = Vec::new();
    let mut extension_updates: Vec<(PathBuf, bool)> = Vec::new();
    for (index, agent) in AGENTS.iter().enumerate() {
        if !names.is_empty() && !names.iter().any(|name| name == agent.name) {
            continue;
        }
        let found = context.found(index);
        let path = context.file(index);
        let extension = matches!(agent.integration, Integration::Extension { .. });
        if extension
            && names.iter().any(|name| name == agent.name)
            && let Some((_, shared)) =
                AGENTS[..index]
                    .iter()
                    .enumerate()
                    .find(|(previous, other)| {
                        matches!(other.integration, Integration::Extension { .. })
                            && names.iter().any(|name| name == other.name)
                            && context.file(*previous) == path
                    })
        {
            let _ = writeln!(
                out,
                "{} and {} share one adapter; uninstalling either removes it for both.",
                shared.display, agent.display
            );
        }
        let previous_update = extension_updates
            .iter()
            .find(|(previous, _)| extension && previous == &path)
            .map(|(_, changed)| *changed);
        let result = (|| {
            if operation != "status" && !names.is_empty() && !found {
                return Err(format!(
                    "{} was not found; looked for {} on PATH and directory {}",
                    agent.display,
                    agent.name,
                    context.directories[index].display()
                ));
            }
            if operation == "install" && !found {
                return Ok(());
            }
            let status = reporting_status(agent, &path, &command)?;
            if operation == "status" {
                let _ = writeln!(
                    out,
                    "{}: {}; reporting {} — {}",
                    agent.display,
                    if found { "found" } else { "not found" },
                    status,
                    display(&path.to_string_lossy(), 800)
                );
                return Ok(());
            }
            if operation == "uninstall"
                && names.is_empty()
                && status == "not installed"
                && previous_update.is_none()
            {
                return Ok(());
            }
            selected += 1;
            let install = operation == "install";
            let update = match agent.integration {
                Integration::Hooks { .. } => settings::update(agent, &path, &command, install)?,
                Integration::Extension { adapter, .. } => match previous_update {
                    Some(changed) => storage::Update {
                        changed,
                        backup: None,
                    },
                    None => {
                        let update = update_extension(&path, adapter, install)?;
                        extension_updates.push((path.clone(), update.changed));
                        update
                    }
                },
            };
            changed_any |= update.changed;
            let message = match (install, update.changed) {
                (true, true) => "installed",
                (true, false) => "already installed",
                (false, true) => "uninstalled",
                (false, false) => "not installed",
            };
            let _ = writeln!(
                out,
                "{}: {} — {}",
                agent.display,
                message,
                display(&path.to_string_lossy(), 800)
            );
            if let Some(backup) = update.backup {
                let _ = writeln!(out, "  Backup: {}", display(&backup.to_string_lossy(), 800));
            }
            if install && update.changed {
                if matches!(agent.integration, Integration::Hooks { .. }) {
                    let _ = writeln!(
                        out,
                        "  Command: {} agent-status hook {} <Event>",
                        display(&command, 800),
                        agent.name
                    );
                }
                if agent.name == "codex" {
                    let _ = writeln!(
                        out,
                        "  Open Codex and run /hooks once to trust the new hooks."
                    );
                    let config = context.directories[index].join("config.toml");
                    if let Ok(Some(raw)) = storage::read(&config)
                        && raw.lines().any(|line| {
                            line.trim_start().starts_with("[hooks")
                                || line.trim_start().starts_with("[[hooks")
                        })
                    {
                        let _ = writeln!(
                            out,
                            "  Note: config.toml contains inline hooks; Codex merges both sources and warns at startup."
                        );
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            errors.push(error);
        }
    }
    let users = definitions::run(
        &definitions.agents,
        context,
        names,
        operation,
        &command,
        out,
    );
    selected += users.selected;
    changed_any |= users.changed;
    errors.extend(users.errors);
    if operation != "status" && selected == 0 && errors.is_empty() && definitions.errors.is_empty()
    {
        let _ = writeln!(
            out,
            "{}",
            if operation == "install" {
                "No supported agents detected."
            } else {
                "No Delux agent reporting installed."
            }
        );
    }
    if operation == "install" && changed_any {
        let _ = writeln!(
            out,
            "Restart already-running agent sessions to load the hooks."
        );
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok(if definitions.errors.is_empty() { 0 } else { 1 })
}

fn reporting_status(agent: &Agent, path: &Path, command: &str) -> Result<&'static str, String> {
    match agent.integration {
        Integration::Hooks { .. } => settings::status(agent, path, command),
        Integration::Extension { adapter, .. } => Ok(match storage::read(path)? {
            Some(raw) if marked(&raw) && raw == adapter => "installed",
            Some(raw) if marked(&raw) => "installed (update available)",
            _ => "not installed",
        }),
    }
}

fn marked(raw: &str) -> bool {
    raw.lines().next() == Some(ADAPTER_MARKER)
}

fn update_extension(path: &Path, adapter: &str, install: bool) -> Result<storage::Update, String> {
    storage::update(path, false, |raw| {
        if raw.is_some_and(|raw| !marked(raw)) {
            return Err(format!(
                "{} is not marked as a Delux adapter; refusing to overwrite or delete it",
                path.display()
            ));
        }
        Ok(if install {
            Some(adapter.to_owned())
        } else {
            None
        })
    })
}
