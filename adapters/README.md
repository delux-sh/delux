# Agent status adapters

Delux shows a session's agent status when something in its tmux pane sets the `@agent_status` option. Built-in integrations wire an agent's own hooks or extension events to that contract.

## The contract

```sh
tmux set-option -p -t "$TMUX_PANE" @agent_status "waiting $(date +%s)"
```

- The value is `working`, `waiting` or `done`, optionally followed by a space and the Unix time it was set. Anything else is ignored.
- `waiting` means the agent needs you: a question, an approval, a review.
- Clear it with `tmux set-option -p -u -t "$TMUX_PANE" @agent_status`.
- Set it on the pane the agent runs in. A session shows the most urgent status among its panes: waiting, then working, then done.
- Include the time. Delux hides `working` once neither the status nor the pane's output has changed for ten minutes, so a crashed agent does not look busy forever.

Any tool that can run a command on its own events can follow this contract.

The generic reporter handles timestamps, pane selection and bounded, silent tmux failures:

```sh
delux agent-status set working   # or waiting, done
delux agent-status clear
```

## Install built-in reporting

Run `delux agents install` on each computer where agents run. It detects Claude Code, Codex and OMP through their executable on `PATH` or configuration directory. `delux agents` (or `delux agents status`) shows detection and reporting status; `install NAME...` selects specific agents, and `uninstall [NAME...]` removes only Delux reporting.

Claude uses `$CLAUDE_CONFIG_DIR/settings.json` or `~/.claude/settings.json`; Codex uses `$CODEX_HOME/hooks.json` or `~/.codex/hooks.json`. JSON settings retain unrelated hooks, key order and duplicate keys, and existing settings files receive a one-time `.delux-backup` before modification. Agent settings are limited to 1 MiB. Codex requires opening `/hooks` once to trust new hooks. Restart already-running sessions after installation. The marked OMP extension is Delux-owned and is replaced or removed without a backup; no lock files are left in agent directories.

## OMP

`omp/delux-agent-status.ts` is an OMP extension. Install it on each computer where OMP runs inside tmux:

```sh
delux agents install omp
```

New OMP sessions pick it up. It reports `working` while the main agent runs, `waiting` while an approval, plan review or question is open, and `done` when the run ends; it clears the status when the session starts and when it shuts down. Subagents do not change the status. Outside tmux it does nothing.

The extension goes in `$PI_CODING_AGENT_DIR/extensions/` when that variable is non-empty; otherwise it goes in `~/${PI_CONFIG_DIR:-.omp}/agent/extensions/`. Reinstallation replaces only adapters carrying Delux's first-line marker. Status reports when the installed marked copy differs from the embedded adapter.

## Adding a built-in agent

Add a plain-data entry to `AGENTS` in `src/agents.rs` with the executable, configuration directory, hook file and event/matcher pairs, then add its event-to-state mapping in `src/agents/reporter.rs` and behavioural tests. Hook handlers call `delux agent-status hook <agent> <event>`; hook payloads never become shell text.
