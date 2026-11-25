# Agent status adapters

Delux shows a session's agent status from OSC 7501 program status on tmux versions that support it (newer than 3.8), or from the pane's `@agent_status` option otherwise. Built-in integrations wire an agent's own hooks or extension events to that option for older tmux and tools that do not emit OSC 7501.

For each pane, a non-empty native status wins when both sources are present, including `idle`, which shows nothing. Delux reads only the program's state, not its name or message.

Native status depends on both sides. tmux records it on its development branch after 3.8; no released tmux does yet. A tool has to emit it inside tmux: Codex and OMP do without being asked, while a tool that first asks the terminal whether it is supported, as Claude Code does, reports only once tmux answers. tmux keeps a status until the program replaces or clears it or the shell prints its next prompt mark, so a tool that exits without clearing can leave `done` showing.

## The contract

```sh
tmux set-option -p -t "$TMUX_PANE" @agent_status "waiting $(date +%s)"
```

- The value is `working`, `waiting` (or `blocked`), `done`, `error` or `idle`, optionally followed by a space and the Unix time it was set. Anything else is ignored.
- `working`, `done`, `error` and `idle` map directly to OSC 7501 states. The option's `waiting` maps to OSC 7501 `blocked`: the agent needs you for a question, an approval or a review. Delux displays either word as `waiting`.
- `error` means failed and stopped. `idle` means at rest and shows no status.
- Clear it with `tmux set-option -p -u -t "$TMUX_PANE" @agent_status`.
- Set it on the pane the agent runs in. A session shows the most urgent status among its panes: waiting, then error, then working, then done.
- Include the time. Delux hides `working` once neither the status nor the session's window output has changed for ten minutes, so a crashed agent does not look busy forever. Background status refreshes also refresh output times for unselected sessions.

Any tool that can run a command on its own events can follow this contract.

The generic reporter handles timestamps, pane selection and bounded, silent tmux failures:

```sh
delux agent-status set working   # or waiting, blocked, done, error
delux agent-status clear
```

The reporter stores `blocked` as `waiting`. Use `clear`, not `set idle`, to remove a reported status.

## Install built-in reporting

Run `delux agents install` on each computer where agents run. It detects Claude Code, Codex, Gemini CLI, OpenCode, Pi and OMP through their executable on `PATH` or configuration directory. `delux agents` (or `delux agents status`) shows detection and reporting status; `install NAME...` selects specific agents, and `uninstall [NAME...]` removes only Delux reporting. User-defined agents are listed after the built-ins.

Claude uses `$CLAUDE_CONFIG_DIR/settings.json` or `~/.claude/settings.json`; Codex uses `$CODEX_HOME/hooks.json` or `~/.codex/hooks.json`. JSON settings retain unrelated hooks, key order and duplicate keys, and existing settings files receive a one-time `.delux-backup` before modification. Agent settings are limited to 1 MiB. Codex requires opening `/hooks` once to trust new hooks. Restart already-running sessions after installation. Marked extensions and plugins are Delux-owned and are replaced or removed without a backup; no lock files are left in agent directories.

Claude's `StopFailure` reports `error`, not successful completion. Hook ownership matches Delux's generated command shape; commands that merely mention the reporter, or combine it with other shell commands, are left untouched.

## Guided setup

In CLI Attention, press `s` (with the sidebar focused), or tap **Set up reporting** below iPhone Coverage. Inspection is read-only and requires Python 3 on the computer being checked; normal session browsing does not.

Setup distinguishes native tmux capability, observed reporting, missing/outdated reporters, installed-but-unconfirmed reporters, and unknown or conflicting configuration. It identifies running agents and their profiles from process metadata, not terminal text. Native capability alone does not prove that a tool is reporting.

Choose a target to review its computer, agent, profile and exact proposed file changes. Confirm explicitly (`y` on CLI, **Install reporter** on iPhone). Pi/OMP extensions and OpenCode plugins are bundled and installed over the existing SSH route without requiring or upgrading remote Delux. Claude, Codex and Gemini hooks require a compatible `delux agents` installer on that computer; otherwise setup gives manual guidance.

Changed processes/profiles/files invalidate a proposal. Unowned files, symlinked paths and hardlinked adapter files are not overwritten. Unsupported or ambiguous configurations remain unknown rather than guessed. An interrupted installation is not retried automatically; inspect again before deciding what to do.

Installation never restarts an agent or replaces the connected terminal. Restart/resume yourself at a safe point. Already-open questions are not replayed, and reporting is confirmed only when a real status arrives.


## Gemini CLI

Run `delux agents install gemini`. Gemini CLI uses `$GEMINI_CLI_HOME/.gemini/settings.json` when `GEMINI_CLI_HOME` is non-empty, otherwise `~/.gemini/settings.json`.

Delux clears the status on `SessionStart` and `SessionEnd`, reports `working` on `BeforeAgent` and `AfterTool`, `waiting` on `Notification` matching `ToolPermission`, and `done` on `AfterAgent`. Gemini's handler timeout is 5000 milliseconds; Claude and Codex retain their five-second timeout fields.

## Pi and OMP

`pi/delux-agent-status.ts` is the shared Pi/OMP extension. Install it on each computer where either runs inside tmux:

```sh
delux agents install pi       # or: delux agents install omp
```

New sessions pick it up. It reports `working` while the main agent runs, `waiting` while an approval, plan review, question or blocking prompt is open, and `done` when the run ends; it clears the status when the session starts and when it shuts down. Subagents do not change the status. Outside tmux it does nothing.

Pi uses `$PI_CODING_AGENT_DIR/extensions/` when non-empty, otherwise `~/.pi/agent/extensions/`. OMP uses `~/${PI_CONFIG_DIR:-.omp}/agent/extensions/` for its default profile, honoring `PI_CODING_AGENT_DIR` except when it is an inherited named-profile directory. A named OMP profile uses `~/${PI_CONFIG_DIR:-.omp}/profiles/<profile>/agent/extensions/` and ignores `PI_CODING_AGENT_DIR`.

The installer resolves `OMP_PROFILE` before `PI_PROFILE`; empty or `default` selects the default profile. Run it with the intended profile:

```sh
OMP_PROFILE=work delux agents install omp
```

Guided setup also reads the running OMP process's `--profile` argument. The standalone installer uses its own environment, not another process's arguments. If Pi and OMP resolve to the same extension directory, Delux installs one identical adapter; uninstalling either removes it for both. Restart/resume an already-running agent at a safe point after installation. Events for questions already open before the adapter loaded are not replayed, so installation alone does not make an existing ask appear in Attention.

## OpenCode

`opencode/delux-agent-status.js` is an OpenCode plugin:

```sh
delux agents install opencode
```

It goes in `$OPENCODE_CONFIG_DIR/plugins/` when that variable is non-empty; otherwise it uses `$XDG_CONFIG_HOME/opencode/plugins/` when `XDG_CONFIG_HOME` is non-empty, or `~/.config/opencode/plugins/`. It reports `working`, `waiting` while a permission request or question is open, and `done`, and ignores sub-agent sessions. Outside tmux it does nothing.

For all three extension/plugin integrations, reinstallation replaces only files carrying Delux's first-line marker. Status reports "update available" when the installed marked copy differs from the embedded adapter.

## Define your own agent

Put a JSON definition in `<delux config dir>/agents/<id>.json`, normally `~/.config/delux/agents/<id>.json`. This is beside the default `cli.json` location and respects `XDG_CONFIG_HOME`; discovery's `--config` flag does not select another definitions directory. The file stem is the agent id: 1–32 lowercase ASCII letters, digits or hyphens, starting with a letter or digit. Built-in names cannot be overridden.

For example, `mytool.json`:

```json
{
  "name": "My Tool",
  "executable": "mytool",
  "file": "~/.mytool/settings.json",
  "events": {
    "SessionStart": "clear",
    "PromptSubmitted": "working",
    "Notification": {"state": "waiting", "matcher": "ToolPermission"},
    "Finished": "done"
  },
  "handler": {"timeout": 5000}
}
```

- `name` is the display name, from 1 to 64 characters.
- `executable` is optional: a bare file name to look for on `PATH`. The agent is detected when that executable is found or the hooks file's parent directory exists.
- `file` is required: an absolute path or one beginning with `~/`. Delux edits this hooks JSON file, not the definition.
- `events` is a non-empty object with at most 32 entries, kept in definition order. Each event name is 1–64 characters without controls. Its value is `working`, `waiting`, `blocked`, `done`, `error` or `clear`, or an object with `state` and an optional string `matcher`. `blocked` is normalized to `waiting`.
- `handler` is optional metadata copied into each command handler. Values must be strings, numbers or booleans; `type` and `command` are reserved. Set timeout units according to the tool's own format.

Definitions are limited to 64 KiB. Unknown top-level fields, invalid definitions and built-in name conflicts are reported without preventing other agents from being processed; they make the command exit non-zero.

Use `delux agents install mytool` or let `delux agents install` include every detected tool. Delux writes `"type": "command"` and either `<delux> agent-status set <state> --agent mytool` or `<delux> agent-status clear --agent mytool`, plus the supplied handler metadata. The label identifies ownership for updates and removal; it does not change the reported state. Unrelated hooks remain untouched, and existing settings files receive the same one-time backup as built-ins.

This supports only tools with the three-level shape `hooks.<Event>[] = {matcher?, hooks: [{type: "command", command, ...}]}`. Each event reports one fixed state, with no conditions or payload interpretation. Other hook formats need a built-in adapter or the tool's own hook calling `delux agent-status set` (and `clear`).

## Adding a built-in agent

Add a plain-data entry to `AGENTS` in `src/agents.rs` with the executable and directory resolution rule. Hook integrations carry a file name and event/matcher pairs; add their event-to-state mapping in `src/agents/reporter.rs`. Extension integrations carry a relative file path and embedded adapter text. Add behavioural tests for either kind. Hook handlers call `delux agent-status hook <agent> <event>`; hook payloads never become shell text.
