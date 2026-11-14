# Verification record

## GPL source-only preparation

The GPL-3.0-or-later source tree passed formatting, Clippy with warnings denied, the macOS ARM64 suite (152 passed; one subprocess helper ignored in the normal run and exercised by its parent), Rust 1.88 all-target checks, Actionlint, and ShellCheck. Gitleaks found no secrets in a copy of the complete publication source tree.

The DCO workflow passed 17 offline cases covering valid and missing author sign-offs, mismatched identities, body-only quotations, pagination limits, incomplete or duplicate results, and a changing PR head/base. It reads GitHub metadata without checking out or executing contributor code; this is not evidence of a live pull-request run.

A native macOS archive built with the GPL grant and retained third-party/toolchain notices; its checksum and contents were verified. Cargo's license metadata and the future Homebrew formula's license and Ruby syntax were checked. Binary distribution and Homebrew remain deferred; packaging smoke artifacts are private and outside the publication tree.

## Browsing previews while connected

The right panel now follows list selection while retaining the connected PTY in the background. Selecting its own session shows the live terminal; selecting another row shows that row's preview. Tab returns to the connected terminal, while Enter opens the selected session.

All 42 picker tests passed, including a regression covering preview delivery with an active client, hidden PTY output draining, and switching back without reconnecting. Formatting, Clippy with warnings denied, and the native macOS ARM64 release build passed.

An isolated real-PTY run with two local tmux sessions verified selection-driven previews, periodic preview refresh, background output, Tab restoring the exact original client, and Enter attaching the selected session. Browsing did not replace the connected client; quitting preserved both sessions. Fixture processes and files were removed. Remote SSH switching was not separately exercised.

## Mouse-wheel forwarding

Delux now captures mouse input and forwards wheel events over the focused terminal using the child's requested mouse protocol and panel-relative coordinates. It never substitutes arrow keys for scrolling. Wheel events over the focused list move selection; unsupported terminal scrolling shows tmux copy-mode/configuration guidance.

Nine focused tests passed, covering real PTY delivery, SGR/legacy/UTF-8 encoding boundaries, modifiers, disabled reporting, queue overflow, coordinate translation, focus, and small layouts. Formatting, Clippy with warnings denied, and the macOS ARM64 release build passed.

An isolated real-PTY run of the rebuilt binary with tmux 3.7b and OMP 18.18.9 verified injected wheel events moved tmux scrollback in both directions and preserved an unsent OMP composer draft. Sidebar scrolling did not scroll the connected terminal, right-panel wheels were ignored with list focus, and disabling tmux mouse support produced guidance rather than arrow-key input. Clean exit disabled mouse capture and preserved both sessions. Fixture processes and files were removed. Physical mouse/trackpad behavior in cmux and remote SSH wheel forwarding were not separately exercised.

## Preview authentication recovery

Preview failures now preserve their status so rejected SSH authentication or host-key verification returns the computer to Sign in or Trust key. Stale sessions and cached previews are removed; successful right-panel authentication restores the session list. Results from an older scan, SSH destination, or session lifetime cannot invalidate the current computer.

All 15 preview-related tests passed, including recovery, stale-result boundaries, local permission failures, and existing capture/refresh tests. Formatting and the native release build completed. A real-PTY/OpenSSH check removed an isolated agent's key and closed only the fixture's shared connection: the picker changed from a live preview to Sign in, Enter opened the right-panel key-passphrase prompt, and successful authentication restored the live preview without echoing the passphrase. Quitting preserved the fixture's tmux session. All fixture processes and files were removed.

## Right-panel sign-in follow-up

SSH access checks now run in the embedded terminal rather than suspending the picker. The 35 picker tests and 13 embedded-terminal tests passed, as did Clippy with warnings denied and Rust 1.88 all-target checks. New coverage checks hidden input, separation and bounded draining of machine-readable stdout, cancellation/refocusing, the existing two-minute access-check timeout, and rejection of results for a changed SSH destination. The obsolete full-screen pause helper and its tests were removed.

The rebuilt macOS ARM64 binary passed an isolated real-PTY/OpenSSH run: unknown-host fingerprint verification, three rejected key passphrases, retry, Ctrl-C cancellation without quitting, Shift-Tab/Enter refocusing, successful encrypted-key authentication, in-place session discovery, and attachment/input over the authenticated connection. No alternate-screen exit occurred during sign-in; the passphrase and inventory protocol never appeared in rendered output, and SSH_ASKPASS was not invoked. The fixture used a key passphrase, not an operating-system account password.

The earlier full-suite and four-archive results below precede both follow-ups. Distribution is deferred; build and verify fresh archives before a binary release.

## Current pre-release review (November 12, 2026)

These results cover the local pre-release development tree. They are not evidence of a published binary release.

- The full macOS ARM64 suite passed: 148 tests, with one subprocess helper ignored in the normal run and exercised by its parent test. After simplifying teardown syntax and making the symlink regression deterministic, the affected storage and embedded-terminal tests passed again.
- Formatting, Clippy with warnings denied, and Rust 1.88 all-target checks passed.
- `cargo audit` reported no known dependency vulnerabilities. Gitleaks found no secrets in the publishable working trees or the CLI's `main` history. These checks are not a security guarantee; the unpushed app/LAN branch is outside this review.
- Actionlint and ShellCheck passed. Release builds now depend on the reusable CI workflow; the changed workflow has not yet run on GitHub.

The review fixed concurrent config-update loss, oversized writes that could not be read back, lossy non-UTF-8 paths, symlink-target mixing, stale previews after tmux ID reuse, delayed recent-attachment rows, clipped pinned computer names, narrow-layout client resizing, literal-dollar session-name selection, and incomplete Homebrew checksums. It also removed redundant child termination and duplicated session-identity checks.

## Platform coverage

| Platform | Latest evidence | Runtime gap |
|---|---|---|
| macOS ARM64 | Full suite, real PTYs and loopback SSH, interactive picker, archive installation through Homebrew | Physical cursor blink timing in cmux was not measured |
| Linux x86_64 | Earlier static musl archive builds; older October suite, discovery, installation, and two-machine attachment passed | Current embedded-terminal and review changes need a fresh Linux run |
| Linux ARM64 | Earlier static musl archive builds; older suite passed on Debian 13 and an archive ran on an aarch64 Yocto board | Current embedded-terminal and review changes need a fresh Linux run |
| Intel macOS | Earlier release archive builds | No Intel Mac runtime test yet |

The Linux cross-linker emitted an upstream warning about an ignored, deprecated optimization setting; both archives built successfully. The previously used Linux workstation rejected batch-mode SSH authentication during this review cycle, so its older results do not verify the new changes.

## Isolated interactive checks

The latest macOS run used temporary config/state and an isolated tmux server:

- At 70×40, filtering the list while attached kept the embedded client's dimensions at 68×17.
- At 100×30, the sidebar stayed 32 columns wide and a long pinned session name left its computer name visible.
- Recent attachments appeared immediately after clearing the filter. Tab and Shift-Tab changed focus, input reached the selected session, and quitting preserved all three fixture sessions.
- `delux attach local '$work'` selected that literal session name, accepted input, and detached successfully.

Earlier isolated macOS runs covered one encrypted-key sign-in shared across previews and an attachment lasting over five minutes; host-key acceptance; paste framing; Unicode; Vim editing; resize; client replacement; detach and session-exit rescans; and same-server native tmux switching. The current automated suite includes the real encrypted-key loopback SSH regression.

Freeze regressions cover stopped children and backpressured PTY cleanup. An earlier live stress run replaced eight deliberately stopped SSH clients without killing their remote sessions. Synchronized-frame checks found no cursor movement or cell painting outside committed frames; that is terminal-protocol evidence, not a measurement of physical blinking.

Upstream tmux 3.2a was previously built and exercised against live preview, attachment, switching, vanished-session, and replacement-server regressions. Homebrew's latest smoke test used tmux 3.8.

## Distribution and Homebrew

Before the GPL cutover, all four release archives built and their SHA-256 checksums verified. Each archive contained the executable, README, project licenses, generated dependency notices, Rust toolchain notices, and musl attribution. Those artifacts are not for publication; packaging and notices must be verified again for a GPL binary release.

The formula generator rejects missing, duplicate, and malformed checksums with a nonzero exit and no partial formula. The generated formula installed the macOS ARM64 archive through a temporary local tap. `brew test` exercised both the version command and JSON discovery of a real isolated tmux session; `brew audit --strict` passed. A sandbox-path issue in the new formula test was fixed before the successful run.

Temporary installations, taps, servers, keys, and interactive fixtures were removed. Homebrew dependency upgrades introduced by the test were rolled back to their previously installed versions. Linux Homebrew installation has not been exercised.

## Before publication

- Require passing CI for the exact source tree intended for publication. Older private CI and distribution runs do not establish readiness of the current tree.
- Complete the runtime checks above, or clearly retain the documented platform limitations.
- Confirm delivery to `security@delux.sh`. Private vulnerability reporting must be enabled when the repository becomes public; it is unavailable while private.
- Configure branch/tag rules and release-environment approvals. Do not publish a release tag while binary distribution and Homebrew are deferred.
- Delux's copyright is held personally by William Chen. Keep the old private history and unreviewed app/LAN work out of the public repository.

Known UI limitation: showing or hiding a filter, notice, or warning can still resize the terminal vertically because those status lines occupy full-width rows. List-content changes alone no longer resize a connected terminal in the stacked layout.
