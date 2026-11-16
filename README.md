# Delux

**Your tmux sessions, across your computers.**

tmux keeps your work running. Delux helps you find it again: browse local and remote sessions in one list, preview what's on screen, and open or create a session in the right-hand panel. Switch back to the list without disconnecting.

![Delux browsing three computers' tmux sessions, opening one in the right-hand panel, and pinning another](docs/demo.gif)

*Recorded in a demo environment with synthetic computers and sessions.*

Delux uses SSH and tmux. It can find computers through [Tailscale](https://tailscale.com), or use SSH destinations you provide. Nothing to install on remote machines beyond the usual SSH and tmux tools.

## Why I built this

Mainly for coding agents. Once I outgrew my laptop, I put old computers to work running agents around the clock, and soon had a lot of tmux sessions to juggle and maintain. Delux is for those of you doing the same with tmux, Tailscale, and SSH.

## Install

For macOS and Linux, with [Homebrew](https://brew.sh):

```sh
brew install delux-sh/tap/delux
```

[Release archives](https://github.com/delux-sh/delux/releases) cover macOS Apple Silicon and Intel, and Linux ARM64 and x86_64. Homebrew installs tmux and, on Linux, OpenSSH; macOS uses its system OpenSSH. If an older manual installation still runs, check `command -v delux` and your `PATH` so it does not shadow Homebrew's binary.

### Build from source

You'll need Rust 1.88+ and a C linker—Xcode Command Line Tools on macOS, or your Linux distribution's build tools:

```sh
git clone https://github.com/delux-sh/delux.git
cd delux
cargo install --path . --locked
export PATH="$HOME/.cargo/bin:$PATH"
```

Keep `~/.cargo/bin` on your shell's `PATH`.

Local discovery and attachment need **tmux 3.2+**. Remote access needs a local OpenSSH client and an SSH server on the remote computer, with **tmux 3.2+**, `base64`, and `tr` available to noninteractive `/bin/sh` commands. No remote Delux service or installation is needed. Tailscale and its CLI are optional, for discovery.

## Use it

```sh
delux                         # local sessions + Tailscale computers
delux --local                 # just this computer
delux --host dev-box          # local sessions + an SSH host or alias
delux list --json              # inventory for scripts
delux attach dev-box project   # full-terminal attachment, without the picker
```

If you don't use Tailscale, start with `--local` or `--host`. You can repeat `--host` and add `--no-local` to show only remote computers. See `delux --help` for all options.

Automatic Tailscale discovery includes only untagged, unshared computers whose known owner matches this computer's known owner. Add other computers explicitly with configured `hosts` or `--host`.

Other VPNs can interfere with Tailscale's DNS and connectivity. If SSH fails while both are active, check your VPN's compatibility with Tailscale; connecting by IP address instead of hostname may not help.

`delux list` prints session columns aligned by display width, with a blank line between computers. It prints available results even if a computer fails, but exits with status 1. Use `--json` for scripts.

### In the picker

| Key | Action |
| --- | --- |
| ↑ / ↓ or `j` / `k` | Move |
| `enter` | Open a session, sign in, or create on an empty ready computer |
| `tab` | Focus the connected terminal; open the selected session if none is connected |
| `shift-tab` | Return to the list without disconnecting |
| `n` | Create a session on the selected computer |
| `/` | Filter computers and sessions |
| `f` | Pin a session |
| `p` | Cycle the session tint |
| `u` | Set the SSH user |
| `a` | Add a computer by SSH destination or alias |
| `r` | Rescan |
| `q` | Quit |

When the terminal has focus, the sidebar turns gray and typing goes to the session. Use `shift-tab` to restore list focus. Browsing other rows previews them without disconnecting the open session. Detach with your tmux prefix followed by `d` (usually Ctrl-B, then `d`). Detaching or quitting Delux leaves sessions running.

The panel title distinguishes **Preview** (Enter to attach), **Connected** (Tab to interact), and **Active** (Shift-Tab for the list). Inactive borders and titles are muted; session output stays readable.

Scroll with the pointer over the focused panel. Scrolling over an inactive preview or terminal shows a centered message in that panel: **Press Enter to attach** or **Press Tab to interact**. A keypress dismisses it; the message does not connect, change focus, or resize the terminal. For tmux scrollback, run `tmux set -g mouse on` on the session's computer, or use copy mode (usually Ctrl-B, then `[`). The list also supports wheel navigation.

**Sign in** and **Trust key** show the **remote username** before connecting. It comes from your saved `user@host` destination or SSH settings/default; confirm it rather than assuming a saved key identifies the account. Ctrl-U changes it; Enter confirms and opens SSH prompts. Only username overrides are saved to Delux. Passwords stay hidden; Ctrl-C cancels sign-in. Verify host-key fingerprints before accepting them.

Press `a` in the list to add an SSH destination such as `alice@build-box` or an alias from `~/.ssh/config`. Enter saves it to `hosts` in the active `cli.json` (including a custom `--config` path), using the destination as both its name and value, just like `--host DEST`. Esc cancels. Delux rescans without reordering existing computers and selects the new computer; adding only probes in the background, never opens an interactive SSH connection. Duplicate names or destinations are rejected. This action is unavailable with `--local` or `--host`, which exclude other configured hosts; restart without those flags to add computers. `--no-local` still allows it.

Duplicate checks include saved computers hidden by `ignore` and mappings saved by another running picker; adding never overwrites an existing mapping.

**New session:** press `n`, or Enter on a ready computer with no sessions. Choose a name, a directory (`~`, `~/path`, or an absolute path), and optionally a command such as `omp`, `claude`, or `codex`. Blank starts the normal shell; a command must be installed on that computer and may end the session when it exits. Tab changes fields; Enter creates and attaches; Esc, Ctrl-C, or Shift-Tab cancels. Sign in first if needed.

### Agent status

On tmux versions that support OSC 7501 program status (newer than 3.8), tools that emit it show their status with no setup. For older tmux versions or tools that do not emit it, run this on each computer where agents run inside tmux:

```sh
delux agents install          # wire detected Claude Code, Codex, Gemini CLI, OpenCode, Pi and OMP
delux agents                  # show what is found and wired
```

Pi and OMP share one adapter; if `PI_CODING_AGENT_DIR` points them at the same directory, uninstalling either removes reporting for both.

Other tools with a compatible hooks file can be described in `~/.config/delux/agents/<id>.json` (or the corresponding `XDG_CONFIG_HOME` directory). See [Define your own agent](adapters/README.md#define-your-own-agent) for the format and limits.

For Codex, open Codex and run `/hooks` once to trust the new hooks. Restart agent sessions that were already running. Use `delux agents install NAME` to select a tool, or `delux agents uninstall [NAME...]` to remove Delux reporting.

For any other tool, call `delux agent-status set working`, `delux agent-status set waiting` (or `blocked`), `delux agent-status set done`, or `delux agent-status set error` from its hooks, and `delux agent-status clear` on session start/end. These commands do nothing outside tmux and silently ignore tmux failures.

The states shown are `working`, `waiting`, `done`, and `error` (failed and stopped); `idle` shows nothing. Native program status takes precedence over installed hooks. Delux shows status in the picker and `delux list`; JSON sessions include `agent` as `null` or an object with `state` and optional timestamp `since` (`null` when absent). The reporter supplies a Unix timestamp; native status has none. A `working` status becomes stale after ten minutes without a newer status or pane output. The [adapter contract](adapters/README.md) also supports setting the tmux option directly.

### A few boundaries

Delux uses the **default tmux server**, not custom `tmux -L` / `-S` servers. Creation is explicit and never replaces an existing session. After a timeout or cancellation, Delux rescans without retrying creation: the session may already exist.

Session ages show when output last changed; explicit agent status separately reports whether an agent is working, waiting, done, or stopped with an error.

The embedded terminal supports keyboard input and mouse-wheel scrolling, but not mouse clicks/dragging, terminal graphics, or advanced keyboard protocols. Use `delux attach` when you need your terminal's full behavior. Inside the same local tmux server, selecting a session switches your current client instead of nesting it.

## Configuration

Use `~/.ssh/config` for users, keys, ports, jump hosts, and aliases. Delux doesn't store passwords or private keys. SSH forwarding is disabled; see the [security model](SECURITY.md#security-model).

To always include or ignore computers, create `~/.config/delux/cli.json`:

```json
{
  "hosts": {
    "dev-linux": "my-linux-ssh-alias",
    "mac-mini": "alice@mac-mini"
  },
  "ignore": ["retired-box"]
}
```

`--config PATH` selects another file. Configuration and state also respect `XDG_CONFIG_HOME` and `XDG_STATE_HOME`. [Discovery rules and local storage](SECURITY.md#discovery-and-local-data) cover shared computers, matching, and saved preferences.

## Contributing

This is a personal side project. Bug reports, small fixes, and ideas are welcome; please open an issue before starting a larger change.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and DCO sign-offs. Report vulnerabilities [privately](SECURITY.md). [Verification notes](docs/verification.md) record tested platforms and remaining gaps.

## Acknowledgements

Delux leans entirely on [tmux](https://github.com/tmux/tmux), [OpenSSH](https://www.openssh.com), and [Tailscale](https://tailscale.com), and is built with [Ratatui](https://ratatui.rs) and [vt100](https://github.com/doy/vt100-rust). Thank you to the people who make them.

## License

[GPL-3.0-or-later](LICENSE). Copyright 2026 William Chen.
