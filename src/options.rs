use crate::workers::default_config_path;
use std::io::Write;

/// A single invocation; SSH details remain in OpenSSH configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub command: String,
    pub config_path: String,
    pub hosts: Vec<String>,
    pub local_only: bool,
    pub no_local: bool,
    pub json: bool,
    pub help: bool,
    pub version: bool,
    pub worker: String,
    pub session: String,
    pub agent_args: Vec<String>,
}

pub fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut positional = Vec::new();
    let mut flags = true;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if flags && arg == "--" {
            flags = false;
            continue;
        }
        if flags && arg.starts_with('-') {
            let (name, supplied) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (arg.as_str(), None),
            };
            match name {
                "--config" | "--host" => {
                    let value = match supplied {
                        Some(value) => value,
                        None => args
                            .next()
                            .map(String::as_str)
                            .ok_or_else(|| format!("{name} needs a value"))?,
                    };
                    if name == "--config" {
                        if value.is_empty() {
                            return Err("--config needs a nonempty path".into());
                        }
                        opts.config_path = value.into();
                    } else {
                        opts.hosts.push(value.into());
                    }
                }
                "--local" | "--no-local" | "--json" | "--help" | "-h" | "--version" | "-V" => {
                    if supplied.is_some() {
                        return Err(format!("{name} does not take a value"));
                    }
                    match name {
                        "--local" => opts.local_only = true,
                        "--no-local" => opts.no_local = true,
                        "--json" => opts.json = true,
                        "--version" | "-V" => opts.version = true,
                        _ => opts.help = true,
                    }
                }
                _ => return Err(format!("unknown option {name}")),
            }
        } else {
            positional.push(arg.as_str());
        }
    }
    if let Some(command) = positional.first() {
        opts.command = (*command).into();
    }
    if opts.help || opts.version {
        return Ok(opts);
    }
    match opts.command.as_str() {
        "agents" | "agent-status" => {
            if !opts.config_path.is_empty()
                || !opts.hosts.is_empty()
                || opts.local_only
                || opts.no_local
                || opts.json
            {
                return Err(format!(
                    "{} does not accept discovery or configuration flags",
                    opts.command
                ));
            }
            crate::agents::validate(&opts.command, &positional[1..])?;
            opts.agent_args = positional[1..]
                .iter()
                .map(|arg| (*arg).to_owned())
                .collect();
            return Ok(opts);
        }
        "" => {}
        "list" => {
            if positional.len() != 1 {
                return Err("list does not take positional arguments".into());
            }
        }
        "attach" => {
            if positional.len() != 3 {
                return Err("usage: delux attach COMPUTER SESSION".into());
            }
            opts.worker = positional[1].into();
            opts.session = positional[2].into();
            if !opts.hosts.is_empty() || opts.local_only || opts.no_local {
                return Err(
                    "attach takes a computer directly; discovery options do not apply".into(),
                );
            }
        }
        _ => {
            return Err(format!(
                "unknown command {} (use list, attach, agents or agent-status)",
                opts.command
            ));
        }
    }
    if opts.json && opts.command != "list" {
        return Err("--json is available with delux list".into());
    }
    if opts.local_only && (opts.no_local || !opts.hosts.is_empty()) {
        return Err("--local cannot be combined with --host or --no-local".into());
    }
    if opts.config_path.is_empty() {
        opts.config_path = default_config_path()?;
    }
    Ok(opts)
}

pub fn print_help(out: &mut dyn Write, command: &str) {
    if command == "attach" {
        let _ = writeln!(
            out,
            "Usage: delux attach [--config PATH] COMPUTER SESSION\n\nAttach to an exact existing session name or '$ID'.\nCOMPUTER is an SSH destination, configured name, or local.\nInside the same local tmux server, switch the current client and exit.\n\n  -h, --help      Show help\n  -V, --version   Show version"
        );
        return;
    }
    let _ = writeln!(
        out,
        "Usage: delux [options]\n       delux list [options] [--json]\n       delux attach [--config PATH] COMPUTER SESSION\n       delux agents [status]\n       delux agents install [claude|codex|opencode|pi|omp ...]\n       delux agents uninstall [claude|codex|opencode|pi|omp ...]\n       delux agent-status set <working|waiting|done> | clear | hook <agent> <event>\n\nFind and attach to existing tmux sessions across your computers.\n\nOptions:\n  --config PATH   CLI JSON configuration path\n  --host DEST     Probe an SSH computer/alias instead of Tailscale (repeatable)\n  --local         Only inspect the local default tmux server\n  --no-local      Do not inspect local sessions\n  --json          Structured inventory and errors (list only)\n  -h, --help      Show help\n  -V, --version   Show version\n\nPicker list: computers contain their sessions; ↑↓ or j/k move.\nEnter opens a session or signs in/checks access on a computer needing setup.\n/ filters computers and sessions, u sets the SSH user, f pins a session,\np cycles tint (cyan, green, amber), r rescans, q quits.\nTab focuses the connected terminal, or opens the selected session if none.\nShift-Tab returns to the list without disconnecting.\nTerminal focus sends normal keys to the session; tmux prefix + d detaches.\nUse delux attach for a full-terminal attachment without the picker.\nSSH configuration and credentials stay in OpenSSH; no Delux service is needed."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_read;

    fn parse(args: &[&str]) -> Result<Options, String> {
        parse_options(&args.iter().map(|arg| (*arg).into()).collect::<Vec<_>>())
    }

    #[test]
    fn discovery_options_can_precede_or_follow_the_command() {
        let _env = env_read();
        let before = parse(&[
            "--host",
            "alice@dev",
            "--config",
            "/custom.json",
            "list",
            "--json",
        ])
        .unwrap();
        let after = parse(&[
            "list",
            "--json",
            "--host",
            "alice@dev",
            "--config",
            "/custom.json",
        ])
        .unwrap();
        assert_eq!(before, after);
        assert_eq!(before.config_path, "/custom.json");
        assert_eq!(before.hosts, ["alice@dev"]);
        let combined = parse(&["--host=a", "list", "--host", "b"]).unwrap();
        assert_eq!(combined.hosts, ["a", "b"]);
    }

    #[test]
    fn contradictory_arguments_cannot_select_another_computer() {
        let _env = env_read();
        let cases: &[&[&str]] = &[
            &["--local", "--host", "dev"],
            &["--no-local", "--local"],
            &["--json"],
            &["attach", "dev"],
            &["list", "extra"],
            &["--config"],
            &["--unknown"],
            &["attach", "dev", "work", "--host", "elsewhere"],
        ];
        for args in cases {
            assert!(
                parse(args).is_err(),
                "accepted contradictory/incomplete arguments: {args:?}"
            );
        }
        let opts = parse(&["attach", "dev", "--", "-session"]).unwrap();
        assert_eq!(opts.session, "-session");
    }

    #[test]
    fn version_flags_skip_config_resolution_and_reject_values() {
        for flag in ["--version", "-V"] {
            let opts = parse(&[flag]).unwrap();
            assert!(opts.version);
            assert!(opts.config_path.is_empty());
            assert!(parse(&[&format!("{flag}=yes")]).is_err());
        }
    }
}
