# Security policy

Delux runs commands on your computers over SSH and renders output from remote tmux sessions in your terminal, so bugs in command construction, host-key handling, or terminal output sanitization can be security issues.

## Reporting a vulnerability

Use [Report a vulnerability](https://github.com/delux-sh/delux/security/advisories/new) on the repository's **Security** tab. Please don't open a public issue or pull request for it.

Include the Delux version (`delux --version`), your OS, and a minimal reproduction, such as a session name, hostname, or remote output that triggers the problem. Remove credentials and unrelated private data; never send passwords, private keys, or tokens. We aim to acknowledge reports within a week.

## Supported versions

Before the first release, fixes land on `main`. After publication, security fixes target the latest release; older versions do not have a separate maintenance branch.

## Security model

### SSH and credentials

Authentication and host keys stay with OpenSSH. Delux reads SSH configuration rather than importing keys or saving passwords. Background scans and previews use `BatchMode=yes` and `StrictHostKeyChecking=yes`: they cannot prompt or accept unknown host keys. Interactive prompts appear only after you choose to sign in or attach. Verify fingerprints through a trusted channel before accepting them.

Sign-in, scans, previews, and attachments reuse a private, per-process SSH control connection where possible. Delux closes these connections on exit; connections with no active channels expire after 30 seconds. An active sign-in or attached client keeps the connection alive, and background probe timeouts do not reset it.

Port, agent, and X11 forwarding are disabled for every Delux SSH operation, including attachments. Use SSH directly when you need forwarding.

### Commands and terminal output

Delux validates SSH destinations, quotes session identifiers in commands, and checks session identity before attaching. A vanished or replaced session is reported rather than silently substituted. Remote login shells must accept POSIX-style command quoting.

Picker previews use read-only `tmux capture-pane`. Both previews and the embedded terminal render parsed output rather than passing escape sequences directly to your terminal. Remote clipboard, window-title, and window-resize requests are ignored. The full-terminal `delux attach` command passes normal tmux output through to your terminal instead.

Agent status is untrusted pane metadata: Delux accepts only the fixed states `working`, `waiting`, and `done` with an optional Unix timestamp. Raw status text is never rendered or executed.

## Discovery and local data

### Which computers are contacted

Automatic Tailscale discovery includes only Linux and macOS peers that are unshared, untagged, and have the same known, nonzero `UserID` as the untagged local node. Missing or zero ownership on either node, a tagged local node or peer, shared peers, and other users' peers require explicit `--host` or configuration. Tailscale presence does not establish SSH access.

For untagged, unshared peers with known matching ownership, host mappings are matched by node ID, full DNS name without its trailing dot, short DNS name, then hostname. Restricted peers match mappings and `ignore` entries only by node ID or full DNS name; their short DNS names and hostnames cannot claim or suppress independently configured aliases. Configure a restricted peer by node ID or full DNS name to associate it with its discovery metadata. Other configured aliases remain independent SSH destinations and are probed even when absent from Tailscale discovery. Saving a user with `u` uses the stable discovery key, taking precedence over name-based mappings.

`ignore` excludes matching computers, including configured hosts. Explicit `--host` destinations bypass discovery and `ignore`, but still resolve through configured mappings.

### Stored preferences

Configuration defaults to `~/.config/delux/cli.json`, including on macOS. Pins live in `~/.local/state/delux/state.json`; legacy recent-attachment entries are preserved but no longer recorded or displayed. `XDG_CONFIG_HOME` and `XDG_STATE_HOME` override those base directories; `--config PATH` overrides the configuration file. Relative or empty XDG directory values are ignored, and non-UTF-8 directory roots are rejected.

These files contain computer mappings and session preferences, not passwords, private keys, or captured terminal output. Previews stay in memory; inventories refresh on rescan, while agent status refreshes independently.

Updates are merged under persistent `.lock` files beside the resolved target files. Do not remove those locks while Delux is running. Existing symlinks are preserved; dangling links are rejected. Each configuration or state file is limited to 64 KiB, and oversized updates are rejected without replacing it.

If saved state cannot be read, Delux reports the error and leaves it untouched; changes to pins remain in memory. Move the unreadable state file aside to start fresh.
