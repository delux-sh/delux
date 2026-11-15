# Verification record

## Separate maintenance refactor

The picker is split into private input, layout, drawing, runtime, and test modules, with model/update logic in `src/tui.rs`. Typed `rustix` calls replace manual production IO, process, socket, and locking boundaries. Seven production `unsafe` blocks remain around `pre_exec`, libc PTY creation/ownership, and zero-initialized termios capture/restore. Test fixtures retain their own OS boundaries. The unreachable expanded-row renderer was removed.

Initial refactor validation, after correcting a signal-constant spelling and a test-only import, passed the full native macOS ARM64 suite: 181 tests, with one subprocess fixture marked ignored. Clippy with warnings denied, Rust 1.88 all-target checking, Intel macOS and both Linux musl architecture checks, and the native release build passed. Dependency license-notice generation passed for that dependency set. Formatting was applied.

The rebuilt binary passed the isolated real-PTY scenario again: aligned inventory, explicit pin/unpin, stable detach/filter order, no channel numbers, complete 60-column hints, embedded shell input, clean exit, and preserved sessions. Cross-target checks are compilation evidence, not native runtime/package validation. No physical trackpad check or release publication was performed.

The preceding release-fixes branch independently passed [native Linux/macOS CI and MSRV](https://github.com/delux-sh/delux/actions/runs/43745509749) and [DCO](https://github.com/delux-sh/delux/actions/runs/43745510204).

Pre-merge review found that the new PTY dependency did not fall back when Linux returned `ENOTTY` for an unsupported `TIOCGPTPEER`. PTY allocation now retains the existing portable libc implementation; the dependency was removed. A Linux subprocess regression uses seccomp to reproduce the unavailable ioctl and requires a working embedded PTY. Only that disposable subprocess installs the filter.

The review fixes passed 181 native macOS tests (one ignored subprocess fixture), Clippy with warnings denied, Rust 1.88 checking, Linux ARM64 musl all-target compilation, and the native release build. The rebuilt picker also passed the real-PTY scenario above. Native execution of the Linux-only seccomp regression is required in CI before merge.

## Release review fixes

Automatic discovery now requires known, matching, nonzero ownership on untagged local/peer nodes and excludes shared peers unless explicitly configured. Regression coverage includes missing/zero ownership, tags on either node, restricted aliases, ignores, and explicit hosts.

The initial test run passed the non-picker checks but timed out on a new test fixture acquiring two attachment guards, and one old scroll assertion expected the wide footer at narrow widths. After splitting the fixture and checking footer preservation instead, all 53 picker tests passed. Clippy with warnings denied, Rust 1.88 all-target checking, the native macOS ARM64 release build, and Actionlint passed; formatting was applied.

A real-PTY scenario with two private tmux sessions verified aligned CLI columns, no initial blank line, no automatic pins after attachment, explicit pin/unpin, stable order after detach/filter clearing, removal of channel numbers, complete 60-column hints, narrow-terminal attachment/input, and session preservation. No real user sessions were touched.

A clean disposable Homebrew 7 prefix installed a dependency-free local formula containing the existing Delux binary using its canonical formula name. Its initially absent trust file acquired only that formula; another formula in the same tap remained untrusted. Sandboxing stayed enabled. Prefixes under `/tmp` failed Homebrew's prefix/sandbox guards, so the successful fixture used a disposable directory outside `/tmp`; all fixtures were removed. This verifies item-scoped install trust, not published download URLs or current four-platform packaging.

Inspection of the raw OS boundaries covered descriptor ownership, IO buffer bounds, initialized terminal/process structures, post-fork restrictions, and kill-before-reap ordering. No additional release-blocking issue was identified in that inspection; it is not a formal safety audit. Module splitting and safe-syscall migration are separate maintenance work.

Pre-merge review found that computers recovering from an empty or failed inventory appended all sessions in scan order. New sessions now append newest-first while existing sessions retain their positions. The expanded refresh/detach/filter regression and the rebuilt binary's isolated real-PTY scenario passed.

## Inactive-panel scroll guidance

The right panel distinguishes Preview (Enter to attach), Connected (Tab to interact), and Active (Shift-Tab for the list). Inactive titles and borders are muted without dimming session output. Wheel input over an inactive panel shows a centered, bordered message over that panel: Press Enter to attach or Press Tab to interact. It does not attach, change focus, overwrite diagnostics or the footer, or resize the terminal. A keypress dismisses it.

All 49 picker tests and four preview-renderer tests passed, along with formatting, Clippy with warnings denied, and the native macOS ARM64 release build. Regression coverage includes previewing a different session while another client remains connected, unattached previews, filtering, pointer bounds, sidebar navigation, and unchanged terminal geometry.

A real-PTY check with two isolated tmux sessions verified all three visible states, injected mouse-wheel hints, explicit attachment/focus, retention of the original client while browsing another preview, hint dismissal, side-by-side and stacked layouts, and preservation of both sessions after quitting. Temporary fixtures were removed. Physical trackpad behavior and remote SSH were not separately exercised. The four-platform packaging results below predate this UI change.

The centered-message follow-up passed 48 picker checks and, after correcting an assertion that assumed every footer says “list focused,” the preview/footer regression passed on rerun. Clippy with warnings denied and the native release build passed. Real-PTY checks observed complete synchronized frames and verified centered messages in side-by-side and stacked layouts, unchanged footer and pane dimensions, key dismissal, and session preservation.

## 0.1.0 Homebrew preparation

[Release dry run 43662052243](https://github.com/delux-sh/delux/actions/runs/43662052243) passed at `f3bd4cc`. It ran Linux/macOS CI and the minimum-Rust check, built all four GPL archives, and verified their checksums on native runners. Every platform installed the generated formula with full dependency resolution, passed `brew test` and `brew audit --strict`, and verified the installed executable was byte-identical to its archive.

Real-PTY checks of those installed executables covered local inventory, embedded attachment, shell input, Shift-Tab focus, native tmux detachment, clean exit, and preservation of the existing session and pane. Each used an isolated default tmux socket and temporary configuration. No real user sessions were touched. The native checks use macOS 15 on both architectures and Ubuntu 24.04 on both architectures; they do not exercise physical mouse/cursor behavior or archived binaries over real remote SSH connections.

The first run caught two formula issues: OpenSSH needs an explicit Linux-only dependency rather than `uses_from_macos`, and the formula's tmux socket must remain inside Homebrew's macOS sandbox test directory. Both were fixed before the successful run. Homebrew reported a Linux Landlock limitation on complete network denial; the isolated local-session checks passed without disabling Homebrew's sandbox.

Actionlint, ShellCheck, a local strict formula audit, and the local PTY smoke harness also passed. Publication was skipped as intended. No tag, binary release, or public formula was created. Later documentation edits add the explicit Homebrew formula-trust instructions and this record; the validated program, formula, and workflow are unchanged.

The older records below retain their original scope; this run supersedes their packaging and native-runtime gaps.

## GPL source-only preparation

The GPL-3.0-or-later source tree passed formatting, Clippy with warnings denied, the macOS ARM64 suite (152 passed; one subprocess helper ignored in the normal run and exercised by its parent), Rust 1.88 all-target checks, Actionlint, and ShellCheck. Gitleaks found no secrets in a copy of the complete publication source tree.

The DCO workflow passed 17 offline cases covering valid and missing author sign-offs, mismatched identities, body-only quotations, pagination limits, incomplete or duplicate results, and a changing PR head/base. It reads GitHub metadata without checking out or executing contributor code; this is not evidence of a live pull-request run.

A native macOS archive built with the GPL grant and retained third-party/toolchain notices; its checksum and contents were verified. Cargo's license metadata and the future Homebrew formula's license and Ruby syntax were checked. Binary distribution and Homebrew remain deferred; packaging smoke artifacts are private and outside the publication tree.

## Sidebar focus contrast

The sidebar uses neutral gray text, borders, status labels, and a subdued selection bar while the terminal or right-panel sign-in form has focus. Returning to the list restores its tint.

The native release build passed. Real-PTY checks verified Tab/Shift-Tab transitions in cyan, green, and amber, plus side-by-side (100×28) and stacked (70×40) layouts. Sidebar colors and the selection highlight changed and restored correctly; the child's red terminal output remained unchanged. Clean exit preserved the session; isolated fixtures were removed.

## Embedded cursor stability

Idle redraws no longer reissue cursor visibility/position commands. Embedded tmux clients request synchronized output; Delux retains a complete cell-and-cursor frame across split reads, with reusable storage and bounded capture work. Unterminated synchronized frames expire after one second. A 50 ms grace period coalesces tmux's unframed cursor-hide prelude; persistent hides still apply, while keyboard handling and sidebar focus remain immediate.

All 21 embedded-terminal tests, three backend regressions, and the live tmux attachment/identity regression passed. Coverage includes silent idle frames, paint-cursor restoration, visibility changes, resize, split DEC 2026 and tmux 3.2 DCS markers, completed frames followed by partial frames, and timeout recovery. Clippy with warnings denied and the native macOS ARM64 release build passed.

The backend clear/redraw regression also passed with disconnected stdin and no controlling terminal. It exercises emitted terminal commands directly rather than requiring CI to supply a physical terminal size; resize remains covered by the embedded-terminal tests and real-PTY checks below.

The cancellation regression verifies that shutdown returns while the runner's registration is still held and that the child receives SIGKILL. It uses a bounded completion signal instead of a 500 ms wall-clock assertion that failed under CI scheduling delays; the focused regression passed.

Isolated real-PTY checks with tmux 3.7b and OMP 18.20.0 verified typing, preserved unsent drafts, Shift-Tab/Tab focus, and resize. OMP typing produced 21 committed updates without cursor hide/show toggles. The final release binary displayed 34 animated updates over two seconds, with deliberately split child frames and no cursor hide/show toggles; input, focus changes, clean exit, and session preservation passed. Fixtures were removed. This verifies terminal output, not physical blink timing in cmux; remote SSH latency and a live tmux 3.2 run were not separately tested.

## Remote username suggestions at sign-in

Interactive sign-in prefills an explicit `user@host` destination or asks OpenSSH (`ssh -G`) to resolve the username asynchronously, including aliases, Includes, and Match rules. The latter is labeled SSH settings/default and requires confirmation, including when SSH falls back to the local account. A key does not identify its remote account. Confirming SSH's suggestion preserves the original destination and does not write a Delux mapping; changing it saves an explicit override. Failed lookups allow manual entry.

All 48 picker tests passed, including unchanged SSH-config preservation, overrides, pending lookup cancellation, stale results, failed persistence, and small layouts. The focused lookup regression passed again after clearing the pending-Enter notice on completion. Formatting, Clippy with warnings denied, and the native macOS ARM64 release build passed.

Real-PTY/loopback OpenSSH checks verified a User supplied by Include and Match final, a different configured account, a key-only alias, and the local fallback without keys. Confirming the configured account preserved the alias, key, port, and unchanged Delux configuration; encrypted-key sign-in, inventory, and session input succeeded without echoing the passphrase. Ctrl-U changed each suggestion. A delayed lookup stayed responsive, cleared its wait notice, and could not overwrite a manual edit. Quitting preserved the fixture session. Fixture processes, keys, and files were removed.

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

## Earlier pre-release review (November 12, 2026)

These results cover the local pre-release development tree. They are not evidence of a published binary release.

- The full macOS ARM64 suite passed: 148 tests, with one subprocess helper ignored in the normal run and exercised by its parent test. After simplifying teardown syntax and making the symlink regression deterministic, the affected storage and embedded-terminal tests passed again.
- Formatting, Clippy with warnings denied, and Rust 1.88 all-target checks passed.
- `cargo audit` reported no known dependency vulnerabilities. Gitleaks found no secrets in the publishable working trees or the CLI's `main` history. These checks are not a security guarantee; the unpushed app/LAN branch is outside this review.
- Actionlint and ShellCheck passed. Release builds now depend on the reusable CI workflow; the changed workflow has not yet run on GitHub.

The review fixed concurrent config-update loss, oversized writes that could not be read back, lossy non-UTF-8 paths, symlink-target mixing, stale previews after tmux ID reuse, delayed recent-attachment rows, clipped pinned computer names, narrow-layout client resizing, literal-dollar session-name selection, and incomplete Homebrew checksums. It also removed redundant child termination and duplicated session-identity checks.

## Platform coverage

| Platform | Latest evidence | Remaining limits |
|---|---|---|
| macOS ARM64 | Native archive installation, formula test/audit, and real-PTY picker checks on macOS 15; source CI and earlier loopback SSH checks | Physical cursor blink timing in cmux was not measured |
| Linux x86_64 | Native musl archive installation, formula test/audit, and real-PTY picker checks on Ubuntu 24.04; Linux source CI | Packaged remote SSH and physical terminal interaction not exercised in this run |
| Linux ARM64 | Native musl archive installation, formula test/audit, and real-PTY picker checks on Ubuntu 24.04 | Packaged remote SSH and physical terminal interaction not exercised in this run |
| Intel macOS | Native archive installation, formula test/audit, and real-PTY picker checks on macOS 15 | Packaged remote SSH and physical terminal interaction not exercised in this run |

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

## Earlier distribution and Homebrew checks

Before the GPL cutover, all four release archives built and their SHA-256 checksums verified. Each archive contained the executable, README, project licenses, generated dependency notices, Rust toolchain notices, and musl attribution. Those artifacts are not for publication; packaging and notices must be verified again for a GPL binary release.

The formula generator rejects missing, duplicate, and malformed checksums with a nonzero exit and no partial formula. The generated formula installed the macOS ARM64 archive through a temporary local tap. `brew test` exercised both the version command and JSON discovery of a real isolated tmux session; `brew audit --strict` passed. A sandbox-path issue in the new formula test was fixed before the successful run.

Temporary installations, taps, servers, keys, and interactive fixtures were removed. Homebrew dependency upgrades introduced by the test were rolled back to their previously installed versions. Linux Homebrew installation has not been exercised.

## Before publication

- Require passing CI for the exact source tree intended for publication. Older private CI and distribution runs do not establish readiness of the current tree.
- Complete the runtime checks above, or clearly retain the documented platform limitations.
- GitHub private vulnerability reporting is enabled and is the reporting channel in `SECURITY.md`.
- Main is protected. Only `replghost` can create `v*` tags; a separate ruleset prohibits updating or deleting them without bypass actors. The `release` environment accepts only `v*` tags, requires `replghost` approval, and disables administrative approval bypass. Do not create a release tag without explicit publication approval.
- Delux's copyright is held personally by William Chen. Keep the old private history and unreviewed app/LAN work out of the public repository.

Known UI limitation: showing or hiding a filter, notice, or warning can still resize the terminal vertically because those status lines occupy full-width rows. List-content changes alone no longer resize a connected terminal in the stacked layout.
