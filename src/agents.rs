mod json;
mod reporter;
mod settings;
mod storage;

#[cfg(test)]
mod settings_tests;
#[cfg(test)]
mod tests;

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::display::display;

pub(crate) use reporter::report;

const OMP_MARKER: &str = "// delux-agent-status adapter. Installed by `delux agents install`; reinstalling replaces this file.";
const OMP_ADAPTER: &str = include_str!("../adapters/omp/delux-agent-status.ts");

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
    },
    Extension,
}

struct Agent {
    name: &'static str,
    display: &'static str,
    executable: &'static str,
    config_env: Option<&'static str>,
    directory: &'static str,
    integration: Integration,
}

// Detection, paths and registration live together so a new built-in has one entry.
const AGENTS: &[Agent] = &[
    Agent {
        name: "claude",
        display: "Claude Code",
        executable: "claude",
        config_env: Some("CLAUDE_CONFIG_DIR"),
        directory: ".claude",
        integration: Integration::Hooks {
            file: "settings.json",
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
        directory: ".codex",
        integration: Integration::Hooks {
            file: "hooks.json",
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
        name: "omp",
        display: "OMP",
        executable: "omp",
        config_env: Some("PI_CODING_AGENT_DIR"),
        directory: ".omp/agent",
        integration: Integration::Extension,
    },
];

pub(crate) fn validate(command: &str, args: &[&str]) -> Result<(), String> {
    if command == "agent-status" {
        return match args {
            ["set", "working" | "waiting" | "done"] | ["clear"] => Ok(()),
            ["hook", ..] => Ok(()),
            _ => Err("usage: delux agent-status set <working|waiting|done> | clear | hook <agent> <event>".into()),
        };
    }
    match args {
        [] | ["status"] => Ok(()),
        ["install" | "uninstall", names @ ..] => {
            for name in names {
                if !AGENTS.iter().any(|agent| agent.name == *name) {
                    return Err(format!(
                        "unknown agent {name}; valid names: {}",
                        AGENTS
                            .iter()
                            .map(|agent| agent.name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            Ok(())
        }
        _ => Err("usage: delux agents [status | install [NAME...] | uninstall [NAME...]]".into()),
    }
}

struct Context {
    directories: Vec<PathBuf>,
    path: OsString,
    executable: PathBuf,
    omp_root_detection: bool,
}

impl Context {
    fn from_inputs(
        home: Option<OsString>,
        path: OsString,
        executable: PathBuf,
        variable: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, String> {
        let omp_root_detection =
            variable("PI_CODING_AGENT_DIR").is_none_or(|value| value.is_empty());
        let directories = AGENTS
            .iter()
            .map(|agent| {
                if let Some(value) = agent
                    .config_env
                    .and_then(&variable)
                    .filter(|value| !value.is_empty())
                {
                    return Ok(PathBuf::from(value));
                }
                if matches!(agent.integration, Integration::Extension) {
                    let config = variable("PI_CONFIG_DIR")
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| ".omp".into());
                    return home
                        .as_ref()
                        .filter(|home| !home.is_empty())
                        .map(|home| PathBuf::from(home).join(config).join("agent"))
                        .ok_or_else(|| "$HOME is not defined".to_owned());
                }
                home.as_ref()
                    .filter(|home| !home.is_empty())
                    .map(|home| PathBuf::from(home).join(agent.directory))
                    .ok_or_else(|| "$HOME is not defined".to_owned())
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            directories,
            path,
            executable,
            omp_root_detection,
        })
    }

    fn environment() -> Result<Self, String> {
        Self::from_inputs(
            std::env::var_os("HOME"),
            std::env::var_os("PATH").unwrap_or_default(),
            std::env::current_exe()
                .map_err(|error| format!("locate running executable: {error}"))?,
            |name| std::env::var_os(name),
        )
    }

    fn found(&self, index: usize) -> bool {
        self.directories[index].is_dir()
            || (matches!(AGENTS[index].integration, Integration::Extension)
                && self.omp_root_detection
                && self.directories[index].parent().is_some_and(Path::is_dir))
            || executable_on_path(&self.path, AGENTS[index].executable).is_some()
    }

    fn file(&self, index: usize) -> PathBuf {
        self.directories[index].join(match AGENTS[index].integration {
            Integration::Hooks { file, .. } => file,
            Integration::Extension => "extensions/delux-agent-status.ts",
        })
    }

    fn command(&self) -> Result<String, String> {
        handler_executable(
            &self.executable,
            executable_on_path(&self.path, "delux").as_deref(),
        )
    }
}

fn executable_on_path(path: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join(name))
        .find(|candidate| {
            candidate.is_file()
                && rustix::fs::access(candidate, rustix::fs::Access::EXEC_OK).is_ok()
        })
}

fn handler_executable(executable: &Path, on_path: Option<&Path>) -> Result<String, String> {
    if let Some(on_path) = on_path
        && let (Ok(current), Ok(found)) = (executable.canonicalize(), on_path.canonicalize())
        && current == found
    {
        return Ok("delux".into());
    }
    if !executable.is_absolute() {
        return Err("running executable path is not absolute".into());
    }
    let executable = executable
        .to_str()
        .ok_or("running executable path is not valid UTF-8")?;
    Ok(format!("'{}'", executable.replace('\'', "'\\''")))
}

pub(crate) fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = Context::environment().and_then(|context| run_in(&context, args, out));
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

fn run_in(context: &Context, args: &[String], out: &mut dyn Write) -> Result<i32, String> {
    let operation = args.first().map_or("status", String::as_str);
    let command = context.command()?;
    let names = args.get(1..).unwrap_or_default();
    let mut selected = 0;
    let mut changed_any = false;
    let mut errors = Vec::new();
    for (index, agent) in AGENTS.iter().enumerate() {
        if !names.is_empty() && !names.iter().any(|name| name == agent.name) {
            continue;
        }
        let found = context.found(index);
        let path = context.file(index);
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
            if operation == "uninstall" && names.is_empty() && status == "not installed" {
                return Ok(());
            }
            selected += 1;
            let install = operation == "install";
            let update = match agent.integration {
                Integration::Hooks { .. } => settings::update(agent, &path, &command, install)?,
                Integration::Extension => update_extension(&path, install)?,
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
    if operation != "status" && selected == 0 && errors.is_empty() {
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
    Ok(0)
}

fn reporting_status(agent: &Agent, path: &Path, command: &str) -> Result<&'static str, String> {
    match agent.integration {
        Integration::Hooks { .. } => settings::status(agent, path, command),
        Integration::Extension => Ok(match storage::read(path)? {
            Some(raw) if marked(&raw) && raw == OMP_ADAPTER => "installed",
            Some(raw) if marked(&raw) => "installed (update available)",
            _ => "not installed",
        }),
    }
}

fn marked(raw: &str) -> bool {
    raw.lines().next() == Some(OMP_MARKER)
}

fn update_extension(path: &Path, install: bool) -> Result<storage::Update, String> {
    storage::update(path, false, |raw| {
        if raw.is_some_and(|raw| !marked(raw)) {
            return Err(format!(
                "{} is not marked as a Delux adapter; refusing to overwrite or delete it",
                path.display()
            ));
        }
        Ok(if install {
            Some(OMP_ADAPTER.to_owned())
        } else {
            None
        })
    })
}
