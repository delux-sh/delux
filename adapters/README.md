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

Run `delux agents install` on each computer where agents run. It detects Claude Code, Codex, OpenCode, Pi and OMP through their executable on `PATH` or configuration directory. `delux agents` (or `delux agents status`) shows detection and reporting status; `install NAME...` selects specific agents, and `uninstall [NAME...]` removes only Delux reporting.

Claude uses `$CLAUDE_CONFIG_DIR/settings.json` or `~/.claude/settings.json`; Codex uses `$CODEX_HOME/hooks.json` or `~/.codex/hooks.json`. JSON settings retain unrelated hooks, key order and duplicate keys, and existing settings files receive a one-time `.delux-backup` before modification. Agent settings are limited to 1 MiB. Codex requires opening `/hooks` once to trust new hooks. Restart already-running sessions after installation. Marked extensions and plugins are Delux-owned and are replaced or removed without a backup; no lock files are left in agent directories.

## Pi and OMP

`pi/delux-agent-status.ts` is the shared Pi/OMP extension. Install it on each computer where either runs inside tmux:

```sh
delux agents install pi       # or: delux agents install omp
```

New sessions pick it up. It reports `working` while the main agent runs, `waiting` while an approval, plan review, question or blocking prompt is open, and `done` when the run ends; it clears the status when the session starts and when it shuts down. Subagents do not change the status. Outside tmux it does nothing.

Both use `$PI_CODING_AGENT_DIR/extensions/` when that variable is non-empty. Otherwise Pi uses `~/.pi/agent/extensions/`, and OMP uses `~/${PI_CONFIG_DIR:-.omp}/agent/extensions/`. If they share a directory, Delux installs one identical adapter rather than two extensions that would both load. Status reads that shared file for both tools, and uninstalling either removes it for both.

## OpenCode

`opencode/delux-agent-status.js` is an OpenCode plugin:

```sh
delux agents install opencode
```

It goes in `$OPENCODE_CONFIG_DIR/plugins/` when that variable is non-empty; otherwise it uses `$XDG_CONFIG_HOME/opencode/plugins/` when `XDG_CONFIG_HOME` is non-empty, or `~/.config/opencode/plugins/`. It reports `working`, `waiting` while a permission request or question is open, and `done`, and ignores sub-agent sessions. Outside tmux it does nothing.

For all three extension/plugin integrations, reinstallation replaces only files carrying Delux's first-line marker. Status reports "update available" when the installed marked copy differs from the embedded adapter.

## Adding a built-in agent

Add a plain-data entry to `AGENTS` in `src/agents.rs` with the executable and directory resolution rule. Hook integrations carry a file name and event/matcher pairs; add their event-to-state mapping in `src/agents/reporter.rs`. Extension integrations carry a relative file path and embedded adapter text. Add behavioural tests for either kind. Hook handlers call `delux agent-status hook <agent> <event>`; hook payloads never become shell text.
