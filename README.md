# Delux

**Your tmux sessions, across your computers.**

tmux keeps your work running. Delux helps you find it again: browse local and remote sessions in one list, preview what's on screen, and open or create a session in the right-hand panel. Switch back to the list without disconnecting.

![Delux browsing three computers' tmux sessions, opening one in the right-hand panel, and pinning another](docs/demo.gif)

*Recorded in a demo environment with synthetic computers and sessions.*

Delux uses SSH and tmux. It can find computers through [Tailscale](https://tailscale.com) and Bonjour/mDNS on your LAN, or use SSH destinations you provide. Nothing to install on remote machines beyond the usual SSH and tmux tools.

## Why I built this

Mainly for coding agents. Once I outgrew my laptop, I put old computers to work running agents around the clock, and soon had a lot of tmux sessions to juggle and maintain. Delux is for those of you doing the same with tmux, Tailscale, and SSH.

## Install

For macOS and Linux, with [Homebrew](https://brew.sh):

```sh
brew install delux-sh/tap/delux
```

[Release archives](https://github.com/delux-sh/delux/releases) cover macOS Apple Silicon and Intel, and Linux ARM64 and x86_64. Homebrew installs tmux and, on Linux, OpenSSH; macOS uses its system OpenSSH. If an older manual installation still runs, check `command -v delux` and your `PATH` so it does not shadow Homebrew's binary.

To upgrade an existing Homebrew installation:

```sh
brew update
brew upgrade delux-sh/tap/delux
```

### Build from source

You'll need Rust 1.88+ and a C linker—Xcode Command Line Tools on macOS, or your Linux distribution's build tools:

```sh
git clone https://github.com/delux-sh/delux.git
cd delux
cargo install --path . --locked
export PATH="$HOME/.cargo/bin:$PATH"
```

Keep `~/.cargo/bin` on your shell's `PATH`.

Local discovery and attachment need **tmux 3.2+**. Remote access needs a local OpenSSH client and an SSH server on the remote computer, with **tmux 3.2+**, `base64`, and `tr` available to noninteractive `/bin/sh` commands. No remote Delux service or installation is needed. Tailscale and its CLI are optional. Automatic LAN discovery requires the computer to advertise `_ssh._tcp` through Bonjour or Avahi; otherwise, configure its address explicitly.

## Use it

```sh
delux                         # local sessions + LAN and Tailscale computers
delux --local                 # just this computer
delux --host dev-box          # local sessions + an SSH host or alias
delux list --json              # inventory for scripts
delux attach dev-box project   # full-terminal attachment, without the picker
```

LAN discovery works without Tailscale. Use `--local` to skip network discovery, or `--host` to use an explicit SSH destination instead of discovery. You can repeat `--host` and add `--no-local` to show only remote computers. See `delux --help` for all options.

Automatic Tailscale discovery includes only untagged, unshared computers whose known owner matches this computer's known owner. Add other computers explicitly with configured `hosts` or `--host`.

An unambiguous LAN advertisement is paired with an existing computer rather than added as a duplicate. Names only nominate routes: an alternate LAN connection must verify the original SSH host identity before authentication. The CLI spends at most three seconds selecting a LAN route before falling back to the normal destination. Existing connections stay on their current route; failed session commands are never replayed on another route.

Other VPNs can interfere with Tailscale's DNS and connectivity. If SSH fails while both are active, check your VPN's compatibility with Tailscale; connecting by IP address instead of hostname may not help.

`delux list` prints session columns aligned by display width, with a blank line between computers. It prints available results even if a computer fails, but exits with status 1. Use `--json` for scripts.

### In the picker

The picker starts in **Browse**, with a **Needs input** section above pinned sessions whenever sessions report waiting. It shows up to nine sessions, each labeled with its computer and a numbered shortcut. With the sidebar focused, press `1`–`9` to open the corresponding session. Numbers remain ordinary input in the terminal, filter and forms.

Press `v`, or click **View all attention** in the sidebar title, for the complete **Attention** view. It collects all sessions explicitly reporting **waiting** across the computers in the current discovery scope; **Errors** is separate and expands automatically when no waiting sessions remain. Opening a session does not clear its status. Both views refresh reported status without replacing or disconnecting an open terminal.

Coverage shows checked, checking, unavailable computers, and sessions without reported status. Missing status is not treated as success or inferred from inactivity. An empty inbox with incomplete coverage is not an all-clear; expand **Computer checks** or return to Browse to investigate.

Press `s` in Attention to **Set up reporting**. Delux checks native support and running agents/profiles, then offers supported reporter installation only after showing the exact files and receiving confirmation. Installed files are not proof of live reporting. See [guided setup](adapters/README.md#guided-setup).

| Key | Action |
| --- | --- |
| ↑ / ↓ or `j` / `k` | Move |
| `enter` | Open a session, sign in, or create on an empty ready computer |
| `tab` | Open the highlighted session, reusing its connection if already attached |
| `shift-tab` | Return to the list without disconnecting |
| `1`–`9` | Open the numbered Needs input session (sidebar focus only) |
| `v` | View all Attention / return to Browse |
| `e` | Expand or collapse errors in Attention |
| `c` | Expand or collapse computer checks in Attention |
| `s` | Inspect and set up reporting in Attention |
| `n` | Create a session on the selected computer |
| `x` | End the selected session, after confirmation |
| `/` | Filter computers and sessions |
| `f` | Pin a session |
| `p` | Cycle the session tint |
| `u` | Set the SSH user |
| `a` | Add a computer by SSH destination or alias |
| `r` | Rescan |
| `q` | Quit |

The **Browse** sidebar has **Needs input**, **Pinned sessions** (shortcuts labeled with their computer), **Computers** (ready or still probing, with sessions nested beneath), and **Other computers** (sign-in, host-key, missing-tool, and connection failures). Nonempty sections have one blank line between them; empty sections are hidden. Pinning adds a shortcut without removing the session from its computer. Status and pin indicators occupy separate columns. Status rows remain selectable; filtering searches computers and sessions, including the inline queue. Status changes move a computer between sections without changing which computer you have selected. A selected session is remembered across a failed probe and restored if it returns.

Computers already reported offline start in **Other computers**, before probing finishes. A successful probe moves them into **Computers** even if Tailscale still reports them offline.

The machine running Delux appears under its operating-system hostname, with a trailing `.local` suffix omitted. Its CLI/configuration identifier remains `local`, so existing pins and mappings keep working. The plain-text `delux list` heading includes both, for example `laptop — local`; use `local` with `delux attach`.

When the terminal has focus, the sidebar turns gray and typing goes to the session. Use `shift-tab` or click inside Browse to restore list focus; click inside a connected terminal to type there again. Clicking a session preview attaches to that session, just like Enter or Tab, including right after launch. Clicking Browse only changes focus—it does not select or open another session. Browsing other rows previews them without disconnecting the open session. Detach with your tmux prefix followed by `d` (usually Ctrl-B, then `d`). Detaching or quitting Delux leaves sessions running.

The panel title distinguishes **Preview** (Enter to attach), **Connected** (Tab to interact), and **Active** (Shift-Tab for the list). Inactive borders and titles are muted; session output stays readable.

Scroll with the pointer over the focused panel. Scrolling over an inactive preview or terminal shows a centered message in that panel: **Press Enter to attach** or **Press Tab to interact**. A keypress dismisses it; the message does not connect, change focus, or resize the terminal. For tmux scrollback, run `tmux set -g mouse on` on the session's computer, or use copy mode (usually Ctrl-B, then `[`). The list also supports wheel navigation.

**Sign in** and **Trust key** show the **remote username** before connecting. It comes from your saved `user@host` destination or SSH settings/default; confirm it rather than assuming a saved key identifies the account. Ctrl-U changes it; Enter confirms and opens SSH prompts. Only username overrides are saved to Delux. Passwords stay hidden; Ctrl-C cancels sign-in. Verify host-key fingerprints before accepting them.

Press `a` in the list to add an SSH destination such as `alice@build-box` or an alias from `~/.ssh/config`. Enter saves it to `hosts` in the active `cli.json` (including a custom `--config` path), using the destination as both its name and value, just like `--host DEST`. Esc cancels. Delux rescans, preserving order within each sidebar section, and selects the new computer; adding only probes in the background, never opens an interactive SSH connection. Duplicate names or destinations are rejected. This action is unavailable with `--local` or `--host`, which exclude other configured hosts; restart without those flags to add computers. `--no-local` still allows it.

Duplicate checks include saved computers hidden by `ignore` and mappings saved by another running picker; adding never overwrites an existing mapping.

**New session:** press `n`, or Enter on a ready computer with no sessions. Choose a name, a directory (`~`, `~/path`, or an absolute path), and **Start**: Shell, OMP, Resume OMP…, Claude, Codex, or Custom command. Delux checks for `omp`, `claude`, and `codex` on the selected computer's command PATH, including over SSH, and shows the installed choices. This checks executable availability, not authentication or account access. Shell and Custom command remain usable during the check. A missing saved launcher stays selected with a warning and cannot be started; choose another launcher explicitly. Lookup failures are shown as errors, not mistaken for missing installations. Reopen New session after changing installations or reconnecting to check again.

If creation interrupts an unfinished tool check and then fails, Delux resumes the check after the rescan finishes and the same computer is ready. This never retries session creation automatically.

**Start** shows the available launchers as indented options, with the selected one marked. Up/Down moves focus without changing the selection; Space selects the focused option. Enter selects it and advances to the next setting. Tab leaves the choices without selecting a different launcher. OMP, Claude and Codex start in the chosen directory; Resume OMP opens OMP's own conversation picker on that computer. An optional OMP profile selects its isolated history and settings; leave it blank to use OMP's normal profile/environment. Custom command can run other installed terminal tools, such as `gemini`.

Select **Use as default** with Space to remember Start and its profile or custom command after successful session creation. The default applies to future sessions on all computers from this Delux client; names and directories are never saved as defaults. One-off changes, cancellation, and failed creation do not replace your saved choice. Shell remains the initial default. The program must be installed and authenticated on the selected computer; when it exits, its tmux session may end. Launching a tool does not install its Attention reporter.

In Directory, **Tab completes paths on the selected computer**—including over SSH—and shows matching directories directly beneath the path, with completion feedback below the matches. Type more to narrow multiple matches, then press Tab again. Feedback distinguishes an existing directory, no matches, and a lookup or connection error. Spaces and special characters are literal; do not shell-escape the path. Lookups happen only when you press Tab. Use Up/Down to move through settings and launcher options; Tab moves to the next setting outside Directory. Enter in a text field or on Use as default advances to the next setting—it never creates a session. When ready, focus **Create session** at the bottom and press Enter or Space to create and attach. Esc, Ctrl-C or Shift-Tab cancels from anywhere. Sign in first if needed.

Long directory matches use their actual wrapped height. When the suggestions exceed the available rows, `… more results` marks the hidden results; type more of the directory name and press Tab to narrow them.

**End session:** with the list focused, press `x`. Check the session and computer in the confirmation: ending a session closes all its windows and panes and can terminate running programs or agents, losing unsaved work. **Cancel** is selected initially; use Tab or an arrow key to select **End session**, then Enter. Esc cancels. Delux verifies the captured session identity before ending it; a replacement is never selected by name. If the connection fails, the operation may already have happened: Delux rescans without automatically retrying. Detach and quit still leave sessions running.

### Agent status

On tmux versions that support OSC 7501 program status (newer than 3.8), tools that emit it show their status with no setup. For older tmux versions or tools that do not emit it, run this on each computer where agents run inside tmux:

```sh
delux agents install          # wire detected Claude Code, Codex, Gemini CLI, OpenCode, Pi and OMP
delux agents                  # show what is found and wired
```

Pi and OMP share one adapter; if their resolved extension directories are the same, uninstalling either removes reporting for both. OMP named profiles are supported: for example, `OMP_PROFILE=work delux agents install omp`.

Other tools with a compatible hooks file can be described in `~/.config/delux/agents/<id>.json` (or the corresponding `XDG_CONFIG_HOME` directory). See [Define your own agent](adapters/README.md#define-your-own-agent) for the format and limits.

For Codex, open Codex and run `/hooks` once to trust the new hooks. Restart agent sessions that were already running. Use `delux agents install NAME` to select a tool, or `delux agents uninstall [NAME...]` to remove Delux reporting.

For any other tool, call `delux agent-status set working`, `delux agent-status set waiting` (or `blocked`), `delux agent-status set done`, or `delux agent-status set error` from its hooks, and `delux agent-status clear` on session start/end. These commands do nothing outside tmux and silently ignore tmux failures.

The states shown are `working`, `waiting`, `done`, and `error` (failed and stopped); `idle` shows nothing. Native program status takes precedence over installed hooks. Delux shows status in the picker and `delux list`; JSON sessions include `agent` as `null` or an object with `state` and optional timestamp `since` (`null` when absent). The reporter supplies a Unix timestamp; native status has none. A `working` status becomes stale after ten minutes without a newer status or pane output. The [adapter contract](adapters/README.md) also supports setting the tmux option directly.

### A few boundaries

Delux uses the **default tmux server**, not custom `tmux -L` / `-S` servers. Creation is explicit and never replaces an existing session. After a timeout or cancellation, Delux rescans without retrying creation: the session may already exist.

Session ages show when output last changed; explicit agent status separately reports whether an agent is working, waiting, done, or stopped with an error.

The embedded terminal supports keyboard input and mouse-wheel scrolling. Clicks focus Delux panes but are not forwarded to terminal applications; dragging, terminal graphics, and advanced keyboard protocols are unsupported. Use `delux attach` when you need your terminal's full behavior. Inside the same local tmux server, selecting a session switches your current client instead of nesting it.

## Configuration

The CLI uses `~/.ssh/config` for users, keys, ports, jump hosts, and aliases. It doesn't store passwords or private keys. SSH forwarding is disabled; see the [security model](SECURITY.md#security-model). The iPhone companion stores its own credentials as described below.

To always include or ignore computers, create `~/.config/delux/cli.json`:

```json
{
  "hosts": {
    "dev-linux": "my-linux-ssh-alias",
    "mac-mini": "alice@mac-mini"
  },
  "lan": {
    "alice@mac-mini": { "hostname": "mac-mini.local", "port": 22 }
  },
  "ignore": ["retired-box"]
}
```

`lan` supplies preferred addresses for computers that do not advertise SSH or whose LAN name differs. Keys are the complete SSH **destination**, including any `user@` prefix—not the display name in `hosts`. The port defaults to 22. Explicit `--host` requests also honor these mappings. Both addresses must present the same trusted host identity. ProxyJump, ProxyCommand, hostname canonicalization and route-dependent SSH settings conservatively retain the normal destination.

`--config PATH` selects another file. Configuration and state also respect `XDG_CONFIG_HOME` and `XDG_STATE_HOME`. [Discovery rules and local storage](SECURITY.md#discovery-and-local-data) cover shared computers, matching, and saved preferences.

## iPhone companion (source build)

`ios/` contains a native SwiftUI dashboard and SwiftTerm terminal over Apple's SwiftNIO SSH. Add a computer by hostname/IP, SSH port and username; see its tmux sessions and agent status, open a terminal, or create a session with a directory and optional command. It connects directly: no Delux daemon or wallet is required on the server.

The dashboard starts in **Attention** and remembers your Attention/Browse choice. It uses the same waiting-only inbox and separate error section described above. Tap a session to respond in its terminal; expand **Coverage** for unavailable computers and unreported sessions, or switch to **Browse** for the full computer/session list.

Tap **Set up reporting** below Coverage to inspect saved computers over SSH and review an installation. Running terminals stay connected; Delux never restarts agents. Pi, OMP and OpenCode reporting can be installed without a Delux binary on the server.

Long-press a session row and choose **End Session…**, or use the terminal's session-actions menu. The confirmation names the session and SSH destination and defaults to **Cancel**. Ending closes every window and pane in that session; leaving the terminal or backgrounding the app does not.

Use a generated per-computer Ed25519 key (copy its public key into that server account's `authorized_keys`) or an SSH password. Credentials stay in the device-only Keychain. Verify the displayed server fingerprint through another trusted channel before connecting. A changed host key blocks access; forgetting a pin is an explicit action in Edit Computer.

**Nearby Computers** in Add/Edit Computer browses LAN SSH advertisements. Use one as a new computer's primary address, or choose **Use for Saved Computer** to pair it with an existing entry without duplicating sessions or credentials. You can also enter an address in **LAN Address (optional)**. Allow Local Network access when iOS asks; discovery alone never signs in.

First-time host-key verification uses the primary address. After a fingerprint is pinned, LAN gets a 750 ms head start; the primary connection starts immediately if LAN fails, or after that delay if LAN stalls. The first fully authenticated connection wins, and both routes must match the same pin before credentials are loaded. The dashboard and terminal show **LAN** when that route wins. Active terminals do not migrate mid-session, and commands are not retried across routes.

To build with Xcode and XcodeGen installed:

```sh
cd ios
xcodegen generate
open Delux.xcodeproj
```

Choose the **Delux** scheme and an iPhone simulator; **Product → Test** runs its tests. Xcode 26.4.1 is the locally tested toolchain. For a physical iPhone, select your signing team in Xcode and enable Developer Mode on the phone. SwiftTerm's pinned build-information plugin needs Xcode approval; command-line builds use `-skipPackagePluginValidation`. Simulator builds still need ad-hoc signing (`CODE_SIGN_IDENTITY=-`), not `CODE_SIGNING_ALLOWED=NO`, because Keychain requires application entitlements. `project.yml` is the project source; regenerate the project after changing it. Package versions are locked in the shared `Package.resolved`.

LAN connections do not require Tailscale. Connect through the separate Tailscale iOS app when the primary address needs it. Desktop SSH aliases, ProxyJump, imported private keys, keyboard-interactive/MFA and automatic tailnet discovery are not supported. The app closes SSH connections in the background and reconnects to the saved session lifetime when reopened; remote tmux sessions keep running. There are no background notifications or universal agent-approval buttons: respond inside the actual terminal. No App Store or TestFlight release is published.

## Contributing

This is a personal side project. Bug reports, small fixes, and ideas are welcome; please open an issue before starting a larger change.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and DCO sign-offs. Report vulnerabilities [privately](SECURITY.md). [Verification notes](docs/verification.md) record tested platforms and remaining gaps.

## Acknowledgements

The CLI uses [tmux](https://github.com/tmux/tmux), [OpenSSH](https://www.openssh.com), [Tailscale](https://tailscale.com), [Ratatui](https://ratatui.rs) and [vt100](https://github.com/doy/vt100-rust). The iPhone companion adds [SwiftTerm](https://github.com/migueldeicaza/SwiftTerm) and [SwiftNIO SSH](https://github.com/apple/swift-nio-ssh). Thank you to the people who make them.

## License

[GPL-3.0-or-later](LICENSE). Copyright 2026 William Chen.
