# ADR-0002 — CLI-first session discovery and attachment

- **Status:** Accepted
- **Date:** 2026-10-19
- **Project:** Delux
- **Supersedes:** [ADR-0001](0001-worker-first-architecture.md) as the product direction and release plan

## Context

The existing Compose application implements manually configured SSH workers, tmux inventory, output previews, and explicit pane input. It does not implement Tailscale peer discovery or interactive terminal attachment.

The immediate need is simpler: type `delux`, find existing work across computers, and resume it in the terminal already in use. A separate graphical control surface adds credential storage, terminal interaction, and platform qualification work before proving that workflow.

The existing daily workflow is cmux with OMP over Mosh/tmux. Delux should remove discovery and reconnection friction without requiring a different agent harness or moving work into another platform.

cmux already documents remote tmux mirroring and Mosh-backed tmux attachment. T3 Code provides graphical clients and server-managed agent threads across remote environments. Kiro Crew provides a persistent agent workspace with memory, scheduled work, and remote gateways. Those managed workflows are alternatives when their capabilities are needed, not prerequisites for Delux. Delux will focus on discovering and attaching to existing terminal work rather than replacing those products.

## Decision

Build a terminal CLI with an interactive session picker as the primary interface. Pause expansion of the Compose clients. They were later removed from this repository so it contains only the CLI.

The first implementation targets Linux and macOS clients and workers. The SSH baseline is implemented in Rust and distributed as a standalone binary; Rust 1.88+ is required to build, not to run. The ratatui picker embeds an OpenSSH/tmux client in a local PTY and renders its VT output, allowing focus to move between the list and the live terminal without disconnecting. System OpenSSH retains authentication and host-key verification; worker-side tmux owns sessions. Direct `delux attach` remains available for a full-terminal attachment. Optional Mosh and native cmux handoff remain separate work; verified behavior is recorded in [docs/verification.md](../verification.md).

### Discovery and access

- Use the local Tailscale CLI's structured peer inventory to discover candidate workers. Include the local computer and permit explicit SSH destinations for machines outside that inventory.
- Tailscale presence and reported online state are hints, not proof of SSH access or tmux availability. Discovery does not enroll machines or grant access.
- Use system OpenSSH for remote inventory and SSH attachment. Reuse SSH configuration, aliases, agent authentication, and host trust rather than importing private keys into Delux. Share a private, per-process authenticated connection across checks, scans, previews, and attachments; disable port, agent, and X11 forwarding for all of them.
- Permit a worker to map to an SSH alias or explicit user/endpoint. Tailscale cannot determine the correct remote Unix account or identity file.
- Background probes must be noninteractive, bounded in duration, output, and concurrency, and cancellable. Show results progressively; one unavailable worker must not block the entire picker.
- Resolve authentication and host-trust prompts only for a worker the user explicitly selects. Do not weaken host verification or automatically accept changed keys.
- Distinguish unreachable workers, access-policy failures where identifiable, authentication failures, host-trust failures, missing tmux, empty inventories, and stale observations. Do not invent a more specific cause than the transport reports.

### Sessions and attachment

- tmux owns session lifetime and terminal history. Delux hosts one interactive client in a PTY and renders its screen through a VT parser. Switching focus does not detach it; selecting another session replaces only the client. Inside a local tmux server, preserve native client switching to avoid recursively displaying the picker.
- Initially query the remote user's default tmux socket. Scope session identity to worker, socket, and server lifetime; do not persist a bare tmux session ID as a durable identity.
- Treat remote names and paths as untrusted data in both command construction and terminal display. Revalidate the selected target before attachment. A disappeared or replaced target must not silently select or create another session.
- Plain local tmux or SSH/tmux attachment is the portable baseline. Detaching or closing the client must leave the remote session running, without forcibly detaching other clients.
- Resume means reattach to a surviving session. It does not mean process recovery after worker reboot, conversation reconstruction, or migration between machines.
- No agent plugin is required. OMP, other coding agents, editors, shells, and long-running commands remain ordinary terminal workloads. Do not infer semantic agent status from terminal text.
- Explicit creation is a separate action: collect name, directory, and optional shell command, create once, capture the new session's lifetime identity, and attach only after verification. Preserve the previous client on failure/cancellation. Never use create-or-attach as recovery for an absent target, and never retry an uncertain remote mutation automatically.

### Optional Mosh

Use SSH for remote discovery and Mosh bootstrap; allow Mosh to carry the interactive terminal. SSH is the default, with an explicit per-worker transport preference.

Mosh requires a compatible local client, remote `mosh-server`, and permitted UDP connectivity. Report missing prerequisites and offer an explicit SSH fallback rather than silently ignoring the preference. Do not install software or change network policy automatically.

Mosh improves roaming and intermittent-connectivity behavior but does not replace tmux persistence. It synchronizes visible terminal state rather than a lossless output stream: local scrollback and arbitrary terminal escape-sequence integrations may differ from SSH. Remote tmux history remains available.

### Optional cmux handoff

Delux owns cross-worker discovery and selection; cmux owns terminal/workspace presentation when the user chooses that integration. Keep ordinary attachment available without cmux.

Use cmux's documented CLI or socket API rather than implementing another tmux control-mode client. Its SSH tmux mirror and Mosh tmux terminal are distinct modes: Mosh attachment must not be presented as native workspace/tab/split mirroring.

An integration must preserve the selected-session contract. In particular, a create-or-attach command is not an acceptable silent substitute for attaching an existing session that disappeared. Check supported APIs and installed versions before enabling a handoff.

cmux's first-class remote workspace path can upload its own helper. Explain that behavior when offering it; neither cmux nor its helper is required for the baseline SSH/tmux workflow. Do not assume identical API support across cmux variants or platforms.

### State ownership

Persist only useful local preferences: worker-to-SSH mappings, ignored workers, pins, recent selections, and transport preferences. Do not duplicate private keys or maintain a competing authoritative session database.

Observed inventories are disposable data from a fresh scan, never cached between runs, and every attachment re-checks the session's identity. tmux remains authoritative even when Delux is not running. No Delux account, background service, or remote Delux daemon is required.

## Relationship to ADR-0001

Retain worker-scoped identity, separation of configuration from observations, explicit host trust, bounded operations, safe target handling, agent independence, and the no-required-remote-Delux-service invariant.

Supersede GUI-first delivery, mandatory imported credentials, the single-selected-worker discovery model, preview/send as the primary interaction, and deferral of full terminal attachment. The old mobile lifecycle and pane-input contracts described the GUI, which has since been removed; they are not requirements to reproduce in the CLI.

Reuse existing tmux encoding, parsing, identity safeguards, and regression cases where applicable. Do not port mobile-specific credential stores, foreground polling, or draft-input coordination solely to preserve code reuse.

## First milestone and acceptance

On two real workers, including Linux and macOS:

1. Launch `delux` and discover workers through Tailscale without manually entering their network addresses; allow required SSH account/alias mappings.
2. Display their existing tmux sessions progressively, alongside local sessions and clear per-worker failures.
3. Select a session and interact with a real shell or OMP using the existing terminal, including keyboard input and terminal resizing.
4. Detach, close the client, and reattach to the same surviving process. Do not terminate the session or evict another attached client.
5. Exercise unknown/changed host keys, failed authentication, unreachable workers, missing tmux, empty inventories, unusual session names, and a session disappearing between selection and attachment. Fail without silently redirecting the user.

Before shipping optional Mosh support, exercise network interruption and recovery with a real remote session, plus missing-client/server and blocked-UDP behavior. Before shipping cmux handoff, exercise the installed API against an existing session and verify that a disappearing target is not recreated. Neither optional path blocks proof of the baseline milestone.

## Deferred work and non-goals

- Session creation followed the original attachment milestone: the picker now creates local/remote sessions explicitly, using the existing SSH and tmux tools. Project presets, directory browsing, and structured agent integrations remain separate work.
- Native mobile clients, graphical dashboards, agent-state interpretation, background monitoring, notifications, scheduling, and cloud provisioning are deferred.
- Adopting arbitrary running terminal processes, custom tmux socket discovery, reboot recovery, and cross-machine session migration are outside the initial scope.
- No T3 provider adapter or partnership is required. T3's authenticated MCP interface is a possible future integration for T3-managed threads, not a tmux attachment protocol.

## Alternatives and consequences

- **Continue GUI-first:** preserves the existing implementation path but delays the daily interactive workflow and overlaps established agent dashboards. Rejected as the immediate priority.
- **cmux-only extension:** avoids some attachment work but ties discovery to one terminal ecosystem. Prefer a portable CLI with optional handoff.
- **Require Mosh:** improves roaming but adds remote software and network prerequisites. Keep it optional.
- **Require a remote Delux daemon:** could provide richer state but adds deployment and security obligations unnecessary for inventory and attachment. Rejected.

This direction reduces Delux's UI and credential-management responsibilities, but SSH onboarding and discovery errors become central product work. Mobile delivery is delayed. The GUI was later removed from this repository rather than kept alongside the CLI.

## References

- [Tailscale CLI](https://tailscale.com/docs/reference/tailscale-cli)
- [cmux remote tmux](https://cmux.com/docs/remote-tmux)
- [cmux SSH and Mosh](https://cmux.com/docs/ssh)
- [Mosh behavior and limitations](https://mosh.org/)
- [T3 Code remote environments](https://github.com/pingdotgg/t3code/blob/main/docs/user/remote-access.md)
- [T3 Code outside-agent MCP interface](https://github.com/pingdotgg/t3code/blob/main/docs/user/outside-agents.md)
- [Kiro Crew capabilities](https://github.com/kirodotdev/KiroCrew/blob/main/src/kiro_crew/docs/index.md)
- [Kiro Crew remote gateways](https://github.com/kirodotdev/KiroCrew/blob/main/src/kiro_crew/docs/remote-crew.md)
