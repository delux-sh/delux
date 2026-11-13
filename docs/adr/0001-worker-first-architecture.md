# ADR-0001 — Worker-first, agent-agnostic architecture

- **Status:** Superseded by [ADR-0002 — CLI-first session discovery and attachment](0002-cli-first-session-discovery.md)
- **Date:** 2026-10-18
- **Project:** Delux

This decision records the architecture of the Compose application, which has since been removed from this repository. ADR-0002 replaces its product direction and release plan, and identifies the safety invariants that remain applicable. The implementation described here is not the CLI.

## Context

Development work spans computers with different resources and toolchains. Delux provides visibility and lightweight control for existing tmux sessions on Linux and macOS workers from Android, iPhone, iPad, and desktop clients.

The initial workflow uses Tailscale for reachability, SSH for authentication and remote operations, tmux for persistent sessions, and OMP as the initial coding-agent workload. Only SSH and tmux are remote prerequisites. Delux neither manages Tailscale enrollment nor requires an OMP integration.

## 1. Worker is the primary entity

A Worker is a computer configured in Delux. Saved configuration consists of a stable ID, display name, hostname/address, port, username, and a credential reference. Private keys and passphrases never belong in worker settings.

Observed connection state, capabilities, session inventory, and timestamps remain distinct from configuration. Unreachable does not mean powered off. Authentication errors, host-trust failures, and network errors must be distinguishable. Previously retrieved observations become stale after failed refresh or disconnection.

## 2. Direct SSH with explicit host trust

No Delux account or remote Delux service is required. SSH command execution supports the first workflow; interactive PTY attachment is deferred.

- Verify the host before sending authentication material.
- Unknown keys require explicit trust and show an endpoint and SHA-256 fingerprint for independent verification.
- Persist trusted fingerprints per endpoint. Changed keys block connection until the user explicitly replaces trust after verification.
- Import SSH private keys into platform-protected storage. Unlock passphrases stay in memory and are not persisted.
- Use Android Keystore-backed encryption, Apple Keychain, Linux Secret Service, and Windows DPAPI. Fail closed if secure storage is unavailable.
- Do not log credentials, submitted input, or captured terminal output by default.
- Tailscale reachability does not replace SSH authorization.
- Bound command duration and captured bytes. Support cancellation without leaking connections.

SSH implementations are platform-specific: SSHJ for Android/JVM desktop, libssh2 for iOS. Shared application code owns trust decisions and application state. Both implementations must meet the same security contract.

## 3. tmux adapter and target identity

v0.1 supports the remote user's default tmux server/socket. Discover sessions, windows, and panes with their relationships, preserving linked windows. Use stable IDs rather than names for targeting, scoped to the worker and server lifetime. Names and paths are untrusted data; encode them rather than assuming they exclude delimiters or newlines.

Distinguish no sessions from missing tmux and command/connection failures. Bind selected panes to server and process identity. Reconnection invalidates selection; require fresh selection before input. Never fall back to an active pane when a target disappears.

Capture bounded recent output as plain text. A preview is not a terminal emulator or structured event stream. Do not infer that an agent completed, failed, or is awaiting input from output alone.

## 4. Explicit input delivery

- Show the destination worker and pane.
- Deliver text literally, not as tmux key names or shell syntax.
- Do not append Enter implicitly; Enter and other supported control keys are separate actions.
- Preserve operation ordering within this client.
- Keep local draft editing independent of refresh activity; polling must not drop keyboard focus. Sending remains gated on fresh observations and serialized operations.
- Keep the editor and control buttons visible above the software keyboard; constrain and scroll worker details rather than letting them displace input.
- Multiline input can execute commands in the receiving application; warn accordingly.
- Never queue input for automatic replay after reconnection.
- Never automatically retry input after timeout or disconnect: delivery may already have occurred.
- Report uncertain delivery distinctly. A successful tmux operation does not mean the application accepted the input or completed its work.

Input is size-bounded; embedded NUL is rejected. Target validation precedes every input operation. Other tmux clients may interact concurrently; Delux does not claim exclusive ownership.

## 5. Bounded foreground refresh

Poll only while foregrounded. Cancel view-specific refreshes when their selection is closed, stop polling on backgrounding, and reject responses belonging to an older selection or connection. Do not overlap refreshes of the same resource. Keep previous observations visibly stale on failure.

Bound output size, command time, and concurrency. Serialize connection operations and avoid refresh backlogs that delay input. Cancelling an input operation does not prove it was not delivered.

## 6. Descriptive capabilities

Discover OS, architecture, and a small read-only set of tools such as Rust, Docker, and Xcode. Distinguish detected, not detected, and unknown/probe failed. Record observation time.

Detection does not establish readiness: a Docker executable does not establish daemon access; Xcode does not establish signing credentials or simulator availability. No capability overrides or automatic placement in v0.1.

## 7. OMP is a workload

Validate OMP in tmux without plugins, lifecycle APIs, or metadata integration. Other agents, shells, editors, and commands use the same operations. No semantic agent-state guarantees in v0.1.

## 8. Compose Multiplatform clients

Use Kotlin Multiplatform and Compose Multiplatform for Android, iOS/iPadOS, and JVM desktop (Linux, macOS, Windows).

Share models, worker management, host-trust policy, tmux operations, capability discovery, refresh/input coordination, and UI. Implement SSH transport, secure storage, lifecycle, and entry points per platform. Adapt layouts and keyboard interaction to the device.

Compose does not make JVM SSH libraries usable on iOS. Use native libssh2 interop there and verify host-key handling and encrypted-key authentication on Apple hardware. iOS builds require a Mac with Xcode; Linux-only builds cannot qualify the Apple target.

Do not introduce a terminal emulator dependency or speculative cross-provider execution framework.

## Application boundaries

| Boundary | Responsibility |
| --- | --- |
| Worker configuration | Persist identity, endpoint, credential references |
| Secure storage and host trust | Protect keys; explicit trust decisions |
| SSH connection | Authenticate, execute bounded operations, report failures |
| tmux adapter | Inventory, previews, explicitly targeted input |
| Capability discovery | Timestamped read-only observations |
| Application state | Selection, foreground refresh, stale state, input outcomes |
| Platform UI | Workers, sessions, explicit user actions |

## Alternatives considered

- **SSHHost or tmux session as root:** mixes saved computer identity with transport/session observations. Use Worker instead.
- **Remote Delux daemon:** adds installation, updates, and a remote security surface. Not required.
- **OMP integration:** narrows the product and adds a dependency. Use ordinary tmux operations.
- **Full terminal first:** adds PTY, rendering, resizing, and keyboard complexity. Prove previews and input first.
- **SwiftUI:** strong Apple integration, but would require a separate Android interface. Compose is chosen because Android and Apple clients are both in the first implementation.
- **Electron:** desktop-only application path does not satisfy the mobile scope.
- **Tauri:** viable shared web UI, but not selected; Compose supplies shared UI without adopting a web frontend.

## Milestones and acceptance

### M1 — Connectivity

Connect to Linux and macOS workers, including over Tailscale. Authenticate using supported imported keys. Unknown hosts require explicit trust; changed hosts block. Distinguish network, authentication, and trust failures. Test encrypted private keys.

### M2 — Discovery

Enumerate real session/window/pane relationships. Handle unusual names/paths and disappearing panes. Distinguish missing tmux, no sessions, and failed discovery.

### M3 — Visibility

Readable bounded previews, nonoverlapping foreground refresh, explicit stale/disconnected state, obsolete-result rejection, and background polling suspension.

### M4 — Control

Literal Unicode, quotes, shell metacharacters, and multiline text; explicit control keys; ordered delivery; target invalidation on reconnect; no silent redirection or automatic retries. Validate with a shell and OMP.

### M5 — Platforms

Exercise connection, inventory, previews, and input on Android, iPhone, iPad, and desktop. Platform compilation is not a substitute for device interaction. Validate platform lifecycle and keyboard behavior.

### M6 — Capabilities

Show OS, architecture, and a small tool set. Distinguish absence from probe failure and retain timestamp/stale information without implying readiness.

## Qualification questions

- Which key formats and authentication combinations pass on both SSH implementations?
- What minimum tmux versions and remote shell environments pass the encoding and targeting checks?
- Which ANSI sequences and output encodings require additional preview handling?
- What polling limits provide acceptable mobile battery and bandwidth use?
- How do multiple clients' simultaneous input and changes affect the user experience?

## Non-goals

Full terminal attachment, session creation, custom tmux sockets, agent lifecycle integration, background monitoring, notifications, scheduling, cloud provisioning, and a Delux account service are outside v0.1.

**Invariant:** A standard SSH-accessible tmux machine remains a fully useful Delux worker without server-side Delux software.
