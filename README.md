# Delux

Delux finds existing tmux sessions on your computers and attaches in your current terminal. It discovers computers through Tailscale or uses SSH destinations you provide. No Delux installation is needed on remote computers.

```text
delux           # full-screen picker; sessions stream in as computers respond
```

## Install

**Source-only preview.** Install from source below. Binary releases and Homebrew installation are not available yet.

### From source

Install Rust 1.88 or newer and a C linker (Xcode Command Line Tools on macOS, or your Linux distribution's C build tools), then run:

```bash
git clone https://github.com/delux-sh/delux.git
cd delux
cargo install --path . --locked
export PATH="$HOME/.cargo/bin:$PATH"
delux --version
```

Cargo's default install directory is `~/.cargo/bin`; add it to your shell's startup file if it isn't already on `PATH`.

### Requirements

- **This computer:** macOS or Linux, OpenSSH for remote access, and an interactive terminal for the picker. Local sessions also require tmux 3.2 or newer, `/bin/sh`, `base64`, and `tr`; use `--no-local` to skip them.
- **Automatic discovery:** a running, connected Tailscale client and its CLI. Without it, local and configured computers are still listed, with a discovery warning.
- **Other computers:** SSH access and tmux 3.2 or newer, `base64`, and `tr` available to noninteractive `/bin/sh` commands. The SSH login shell must accept POSIX-style command quoting. No Delux installation.

Delux inspects the current user's **default tmux server** on each computer, not custom `tmux -L` or `-S` servers, and does not create sessions. Start a session with tmux first.

## Usage

```bash
delux                            # picker over local sessions and discovered computers
delux --local                    # local sessions only
delux --host dev-box             # local + one SSH destination, no Tailscale scan
delux --host dev-box --host alice@build-box --no-local
                                 # only these two SSH destinations
delux list                       # print sessions and per-computer errors
delux list --json                # the same as JSON
delux attach dev-box project     # attach to an exact session name
delux attach local '$3'          # or an exact tmux session ID (quote it)
```

`--host DEST` is repeatable and replaces the Tailscale/configured-host scan; configured mappings still resolve each supplied name. `--local` cannot be combined with `--host` or `--no-local`. `attach` takes an SSH destination, configured name, or `local` directly; discovery flags do not apply. Use `delux --help` or `delux attach --help` for the full syntax.

The picker has one combined list of computers and their sessions. Use ↑/↓ or `j`/`k` to move, `enter` to open a session or check a computer's access, `/` to filter computers and sessions, `r` to rescan, and `q` to quit. While editing a filter, `enter` finishes editing and `esc` clears it. You can pick a session before the scan finishes.

- **Sessions** are grouped by computer, with recently used sessions first within each group. Rows show the age of the last output; this is not a reliable indication that a program is busy or finished. The preview's bottom border shows the active pane's command and working directory, output age, last use, windows, and attached clients. Small terminals show these details in the rows instead.
- **Computers** with sessions appear first, followed by those needing setup, probing or empty computers, other failures, and offline computers last. All computer names share the same left edge and heading style; only sessions are indented. Computers without sessions remain as compact rows with status indicators after their names. Select **Sign in** or **Trust key** and press `enter` to open OpenSSH's prompts in the right-hand panel. Successful sign-in expands that computer's sessions in the list. `u` saves an SSH user for the selected computer or session.
- **Preview:** with enough terminal space, the selected session's active pane is captured after the selection rests for a quarter second, preserving supported colors and text attributes. At 80 columns or wider, a fixed 32-column sidebar leaves at least 48 columns for the preview. Filtering, scanning, and changing selections do not resize the sidebar. Narrower, taller terminals stack the panels: previews give the list only the rows it needs, up to half the height; a connected terminal keeps a half-height split so list changes don't resize it. Longer lists scroll, and Page Up/Down follows the list's visible height. Successful previews normally refresh every second; failed captures or remote connections without Delux connection sharing refresh every five seconds. If SSH authentication or host-key verification fails, the computer returns to **Sign in** or **Trust key** instead of continuing to list stale sessions; press `enter` to restore access in the right panel.
- **Terminal:** opening a session replaces the preview with an interactive SSH/tmux terminal. From the list, `tab` focuses the connected terminal, or opens the selected session if none is connected. `enter` opens the selected session, replacing the previous client if necessary. `shift-tab` always returns to the list without disconnecting. With terminal focus, normal typing, `tab`, paste, and Ctrl-C go to the session, not the picker. Browsing the list leaves the active session connected; opening another session detaches only the previous client. On small screens, the focused panel takes the available space. To quit Delux, return focus to the list first.
- **Pins:** `f` pins a session. Pinned sessions and your five most recent attachments are listed first.
- **Tint:** the selector uses your terminal's cyan; `p` switches it to green or amber phosphor.

During sign-in, passwords and key passphrases remain hidden and are handled by OpenSSH; Delux does not save credentials. `shift-tab` returns to the list, and `tab` or `enter` on that computer refocuses the pending prompt. Ctrl-C while the sign-in panel is focused cancels just that login, not Delux. Starting sign-in replaces any existing embedded client without stopping its tmux session. Verify host-key fingerprints through a trusted channel before accepting them.

Detach with your tmux prefix followed by `d` (default Ctrl-B, then lowercase `d`); the panel returns to a preview and the inventory refreshes. The session keeps running, and other attached clients aren't disconnected. Use tmux copy mode for history. The embedded terminal does not implement mouse input, terminal graphics, or advanced keyboard protocols; `delux attach COMPUTER SESSION` provides a normal full-terminal attachment instead.

When Delux runs inside tmux, picking a local session on that same server switches your current tmux client and exits Delux, rather than nesting the session containing the picker. A local session on a different server still requires detaching first. Remote sessions remain embedded; if the outer tmux intercepts your prefix, send it twice to reach the inner session.

`delux list` exits with status 1 if discovery or any computer failed, while still printing everything it found. Without the Tailscale CLI, discovery counts as failed; use `--local` or `--host` to skip it.

## Configuration

Put usernames, ports, keys, jump hosts, and aliases in `~/.ssh/config`, or press `u` in the picker to save a user for one computer. Delux uses OpenSSH for authentication and does not store passwords or private keys. Nonstandard ports belong in SSH config, not `--host host:port`.

Optional settings live in `$XDG_CONFIG_HOME/delux/cli.json` (default `~/.config/delux/cli.json`, on macOS too); `--config PATH` overrides it. Relative or empty XDG directory values are ignored. Absolute directory roots must be valid UTF-8; unsupported paths are reported rather than rewritten.

```json
{
  "hosts": {
    "dev-linux": "my-linux-ssh-alias",
    "mac-mini": "alice@mac-mini"
  },
  "ignore": ["retired-box"]
}
```

`hosts` maps a computer to an SSH destination. Discovery matches keys by Tailscale node ID, full DNS name (without the trailing dot), short DNS name, then hostname, in that order. `ignore` excludes matching computers, including standalone configured entries. Computers listed in `hosts` are probed even when Tailscale doesn't report them, unless ignored. `--host` bypasses discovery and `ignore` entirely.

Saving a user with `u` writes the computer's stable discovery key, so it takes precedence over older name-based mappings. Concurrent mapping updates are merged under a persistent `cli.json.lock` beside the config-file target; don't remove it while Delux is running. Existing config-file symlinks are preserved; dangling links are reported rather than replaced.

Discovery probes Linux and macOS computers that belong to you, plus tagged devices (Tailscale gives those no human owner to compare). Computers shared into your tailnet and other users' devices are skipped unless you list them in `hosts` by their node ID or full DNS name; a shorter name doesn't count for them, so a foreign device can't take over one of your entries by renaming itself. If Delux itself runs on a tagged device, the owner check is skipped.

Pins and recent attachments are stored in `$XDG_STATE_HOME/delux/state.json` (default `~/.local/state/delux/state.json`). Updates from multiple pickers are merged under a persistent `.lock` file beside the state-file target; don't remove that lock while Delux is running. State-file symlinks are preserved. If state can't be read, Delux says so and leaves it untouched; pins can still change in memory. Move the unreadable file aside to start fresh.

Config and state files are limited to 64 KiB each. An update that would exceed the limit is rejected without replacing the previous file.

## Security model

- **Host keys and authentication stay with OpenSSH.** Background scans and previews use `BatchMode=yes` and `StrictHostKeyChecking=yes`, so they never prompt or accept unknown host keys. SSH runs interactively only when you act: attaching to a session, `enter` on a computer's status row, or `delux attach` to a computer that needs a host-key check or a login.
- **Sign-in, scans, previews, and attachments share a connection where possible.** Interactive access checks establish the same private, per-process OpenSSH control connection used by later operations, without Delux storing credentials. A sign-in or attached client keeps that connection alive while you browse the list; background probe timeouts do not reset it. Delux closes its control connections on exit. Connections with no active channels expire after 30 seconds.
- **Forwarding is disabled for every Delux SSH operation.** Port, agent, and X11 forwarding are disabled, including attachments. Use SSH directly when you need forwarding.
- **Session data is not shell code.** SSH destinations are validated, and tmux session identifiers are passed through quoted commands.
- **Picker output is parsed, not passed through.** Previews use read-only `tmux capture-pane` and interpret only supported colors and text attributes. The embedded terminal renders parsed VT cells; remote clipboard, window-title, and window-resize requests are ignored. `delux attach` passes normal tmux output to your terminal.
- **Inventories are refreshed on rescan.** Preview data is held in memory, not saved to disk. Attaching checks the selected session's identity so a vanished or replaced session is reported rather than silently substituted.

Report vulnerabilities privately; see [SECURITY.md](SECURITY.md).

## Status

Delux is CLI-only, for macOS and Linux. Recorded manual runs and known verification gaps are in [docs/verification.md](docs/verification.md); CI runs the suite on Linux x86_64 and macOS ARM64. Intel Mac runtime testing is still outstanding. There is no GUI, LAN discovery, Mosh transport, or session creation in this preview. See [ADR-0002](docs/adr/0002-cli-first-session-discovery.md) for the design.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The tests use real tmux servers on isolated temporary sockets and real PTYs, never your own sessions. `scripts/dist.sh` builds archives into `dist/`: on macOS all four targets, using [zig](https://ziglang.org) and [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) for Linux; on Linux the two Linux targets. Packaging also uses [cargo-about](https://github.com/EmbarkStudios/cargo-about) to collect dependency licenses; see [CONTRIBUTING.md](CONTRIBUTING.md#source-only-launch-and-deferred-packaging).

Manually dispatching the Release workflow runs CI and builds a `delux-dist` artifact without publishing. In a public repository, pushing a `v<version>` tag matching `Cargo.toml` triggers CI, builds, and GitHub Release publication through the `release` environment; updating the Homebrew tap also requires its token. Private repositories cannot publish through this workflow. Keep pre-release checks on the manual workflow, without pushing a tag.

See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## License

Copyright (c) 2026 William Chen. Licensed under the [GNU General Public License, version 3 or later](LICENSE) (`GPL-3.0-or-later`). Contributions use the same license and require a [DCO sign-off](CONTRIBUTING.md#developer-certificate-of-origin); contributors retain ownership of their contributions.

The packaging tooling includes dependency notices in `THIRD-PARTY-NOTICES.txt`, the upstream Rust toolchain license inventory in `RUST-NOTICES.html`, and musl's `MUSL-COPYRIGHT` for Linux builds. The Rust inventory also covers compiler and optional components not linked into Delux.
