# Verification record

## CLI new-session directory completion

Tab in Directory performs a read-only lookup on the selected computer, locally or over existing noninteractive SSH. Unique matches fill a trailing slash; complete ambiguous results extend the common prefix and list choices. Partial results never infer a prefix. Up/Down changes fields. Editing, submitting or closing the form invalidates pending results; reopening an identical form cannot accept an earlier reply.

- Eight directory-lookup regressions passed, covering literal shell metacharacters, Unicode/spaces, hidden entries, directory symlinks, missing/inaccessible parents, bounded replies and invalid protocol data. One initial fixture attempted a non-UTF-8 filename on APFS; that byte-validity check now exercises the wire parser directly, while control-character filenames still exercise the actual shell.
- All 15 form tests and 107 CLI component tests passed, including delayed responses, edit-back invalidation, changed remote identities, truncated matches and tiny viewports. Formatting and the macOS ARM64 release build passed.
- An isolated running release displayed ambiguous and missing local paths, completed spaces and literal command-substitution text without executing it, and created a local session in the selected directory. A real loopback SSH connection completed and created a session on the remote fixture in its selected directory; it did not create that session on the local fixture. Completion preserved the already-attached remote client.
- The actual CLI was inspected at 130×34 and 55×15. The smaller layout retained the focused path and existence feedback. A controlled SSH failure displayed a lookup error distinct from no matches; cancellation created no session. Private tmux/SSH fixtures and the throwaway smoke script were removed.

This change is CLI-only. It has not been published to Homebrew and does not change iPhone directory entry.

## v0.3.0 release (November 25, 2026)

[Delux v0.3.0](https://github.com/delux-sh/delux/releases/tag/v0.3.0) was published from approved commit `f7f48acb59340fd7b6081147520a55b25c7cfa0c`, merged into protected main through PR #19.

The [nonpublishing validation](https://github.com/delux-sh/delux/actions/runs/45542367764) and [tagged release workflow](https://github.com/delux-sh/delux/actions/runs/45547573084) both passed Rust CI, minimum-version checks, archive generation and all four native package jobs: macOS Apple Silicon/Intel and Linux ARM64/x86_64. Each installed the generated formula with dependencies, ran `brew test` and `brew audit --strict`, and exercised the installed picker in isolated tmux sessions. Intel macOS built dependencies from source; no architecture was emulated or skipped.

Downloaded validation artifacts had matching checksums, complete license notices and the committed release notes. After the release-environment gate was explicitly approved, publication and provenance attestation succeeded. All four archives were downloaded again from the published release and checked against its published `SHA256SUMS`; those checksums, not the dry-run artifacts, generated the [tap update](https://github.com/delux-sh/homebrew-tap/pull/5).

The existing agent-reporting and iPhone limitations below remain unchanged; package verification does not establish real model-generated OMP asks or iOS binary distribution.

## Inline Attention in CLI Browse

Browse now places up to nine numbered waiting sessions above pins and computers, hides that section when empty, and keeps the full Attention view on `v`. Numeric shortcuts use the displayed filtered queue; terminal, form and filter input retain precedence. Status and pin columns are separate, and nonempty Browse sections have one blank row between them.

- The CLI component run passed 103 tests. One pinned-layout test over-specified the exact truncation point; that assertion was removed while retaining host-visibility and spacing checks, and the remaining test passed in a focused rerun.
- New regressions cover the nine-shortcut limit, same-named sessions on different computers, filtered numbering, missing shortcuts, and removal without silently retargeting Enter. Updated polling coverage verifies unshared remote status refresh and recovery directly in Browse.
- Formatting and the macOS ARM64 release build passed.
- A private real-PTY/OpenSSH/tmux scenario displayed two waiting sessions, pinned one through the UI, and verified single-row section spacing and separated markers. Shortcut `1` opened the correct remote computer; a digit typed in its terminal reached the shell. Clearing both waiting states hid the section without replacing the attached SSH client, and a later waiting state appeared without switching views. Filter input and `v` navigation also worked.
- The running release was inspected at 130, 70 and 32 columns. Fixtures were removed afterward. These were controlled tmux statuses, not a real model-generated OMP question; iPhone behavior was not changed.

## Guided reporting setup: CLI and iPhone

Attention now offers read-only diagnosis and explicit, file-by-file reporter installation review. The shared Python backend distinguishes tmux native capability from observed reporting, resolves running agent profiles, and installs bundled Pi/OMP/OpenCode adapters over the existing connection. Other built-in hooks require a compatible remote Delux installer.

- Python backend: 16 isolated tests passed, covering profile resolution, native versus observed status, installed-but-unconfirmed reporting, stale/forged proposals, ownership and symlink/hardlink refusal, and atomic installation/update.
- Rust: 12 reporting tests, nine extension installer tests and nine Attention tests passed. Formatting and the macOS ARM64 release build passed.
- iPhone: all 26 selected reporting-setup and AppModel tests passed on the iPhone 17 Pro simulator. The final simulator build was installed and visually inspected.
- Isolated real OpenSSH/tmux fixtures exercised CLI and iPhone diagnosis of missing OMP reporters in separate named profiles. Cancellation left files absent; explicit confirmation installed the exact packaged adapter. The agent process remained running and no status was invented.
- A confirmed CLI update preserved the established SSH client and agent PIDs; subsequent terminal input reached the original pane. The check waited for an actual attached client before recording its identity.
- After explicitly restarting only the private fixtures, a Bun harness invoked the installed adapter's ask/answer handlers. CLI Attention entered and cleared its waiting state; iPhone showed the waiting session and setup subsequently showed reporting confirmed. This verifies real adapter-to-tmux-to-UI delivery, not a model-generated question.

No user agent was restarted or sent input. These checks do not establish physical-iPhone installation or Homebrew publication. Setup requires Python 3 on the inspected computer; normal browsing does not. Installed reporters do not replay already-open questions.

## OMP named-profile reporting setup

A reported missing Attention entry was traced to a remote OMP 19.1.0 session running under tmux 3.8 with neither native program status nor `@agent_status` populated. No Delux adapter was installed, and the remote Delux 0.1.0 binary did not have the `agents` installer. The packaged Pi/OMP adapter was installed directly into the active named profile's extension directory.

An isolated remote tmux server and Bun harness exercised that installed adapter's event handlers: session start cleared status, agent start reported working, an ask tool call reported waiting, its result restored working, agent end reported done, and shutdown cleared status. The private server was removed afterward. This verifies adapter-to-tmux reporting, not an actual model-generated ask. The existing user session was not restarted or given input; its already-open question was not claimed to appear retroactively.

## Attention dashboard: CLI and iPhone

The source tree now provides a cross-computer waiting-session inbox, separate collapsible errors, and explicit coverage for checking/unavailable computers and unreported status. Browse remains the CLI default; iPhone starts in Attention and remembers the selected view.

- `cargo test tui::tests`: 93 passed. After correcting unnecessarily truncated computer labels, all nine `tui::tests::attention_` regressions passed. Native debug build and formatting check passed.
- The CLI regressions cover cross-computer identity, stable queue selection, reused tmux lifetimes, preserved connected terminals, separate errors, incomplete coverage, and bounded polling/failure/recovery for unshared remote computers.
- iPhone simulator build and all 17 `AppModelTests` passed, including six new inbox/coverage/lifetime regressions. Later presentation and accessibility changes were rebuilt and checked through the running app.
- An isolated real-PTY/OpenSSH/tmux run displayed two identically named waiting sessions on distinct computers, with both computer names readable in the normal sidebar. Opening the selected session delivered input to the correct computer. Changing its status to working removed it from Attention without replacing the connected client; subsequent input still reached that session. Clicking Browse/Attention preserved terminal focus and the same client. Error and unavailable-computer disclosures worked, and quitting preserved the remote session.
- A separate iPhone simulator connected through real loopback SSH, using computers and verified host keys added through the UI. It showed two waiting sessions, separate errors, and incomplete coverage. Input reached the selected computer before and after its waiting status cleared; the terminal remained connected. Returning showed the reduced queue. Clearing both waiting states automatically exposed errors without claiming all-clear. Coverage navigation, error disclosure accessibility, and remembered Browse selection after relaunch were exercised.

These checks used only private fixture sessions and a dedicated simulator, not the user's existing sessions or simulator. They do not verify physical-iPhone installation, background notifications, or a published release.

## LAN-first connectivity and iPhone rotation (November 21, 2026)

The CLI browses Bonjour/mDNS SSH advertisements, merges unambiguous matches with existing computers, and supports explicit `lan` mappings keyed by canonical SSH destination. Alternate routes preserve the original OpenSSH trust identity. Route selection sends no session command; failed mutations are not replayed. The iPhone app adds explicit nearby-computer pairing and a pinned-key LAN-first connection race.

The 14 LAN-focused Rust tests, 23 worker tests, seven mux tests and 15 app tests passed. Isolated OpenSSH coverage includes stalled and wrong-key routes, unknown-key rejection, multiple advertised endpoints, encrypted-key embedded sign-in with an unreachable primary, nonblocking route readers, stale-result rejection and no mutation replay. Formatting, Clippy with warnings denied and Rust 1.88 all-target checking passed. A freshly built CLI inventoried the private `lan-review` tmux session through the configured LAN interface; captured SSH arguments retained the canonical host-key alias and selected the LAN address.

All 26 iPhone model, credential, protocol and route/authentication tests passed before the keyboard-layout follow-up. An isolated iPhone 17 Pro simulator without Tailscale discovered a real `_ssh._tcp` advertisement, verified the fixture host key, and paired the service with one existing computer without duplicating it. Live inventory and terminal input worked through the host's LAN address, confirmed by `LAN_ADDRESS_OK` in the private tmux session. A silent SSH endpoint and a different host key each fell back to the trusted primary address. The wrong-key sshd logged a disconnect without attempting authentication.

Rotation exposed an accessory-overlap regression: UIKit's keyboard layout guide under-reported the keyboard's coverage after the accessory safe-area inset changed. The terminal now uses keyboard end-frame notifications and separately respects its bottom safe area. Inspected screenshots showed no overlap in portrait and landscape with software and hardware keyboards. The portrait software-keyboard viewport ended at y=522, above buttons at y=526–562; landscape hardware mode ended at y=338, above buttons at y=342–378. Dismissing the landscape keyboard restored the viewport to y=382. Remote input still worked after rotation (`ROTATION_INPUT_OK`).

Two new UIKit geometry regressions passed, cycling software, hardware and dismissed keyboard frames in portrait and landscape. Their initial fixture omitted a `UIWindowScene`; attaching the test window to the app's scene corrected the fixture without weakening the viewport assertions.

The connected physical iPhone was paired, unlocked and in Developer Mode, but this Mac had no valid signing identity or configured development team. Nothing was installed on the phone. Physical-device, cellular/Tailscale transitions and remote physical-host coverage remain unverified; these network scenarios used isolated sshd/tmux fixtures on the development Mac. No release or Homebrew publication is included.

The temporary simulator, fixture credentials, SSH/Bonjour listeners, private tmux server and smoke scripts were removed. The detached tmux server required explicit cleanup after stopping the supervised fixture. Screenshots remain outside the checkout; no user sessions were changed.

## Sidebar availability grouping (November 20, 2026)

The Browse sidebar separates Pinned sessions, Computers, and Other computers with blank lines and omits empty sections. Pinned shortcuts retain their computer label and their ordinary nested session row. Ready and probing computers remain in Computers; authentication, host-trust, missing-tool and connection failures appear in Other computers. Ordering within each section is preserved between explicit rescans; selection follows a computer when its sessions disappear or access returns. Navigation skips section headings and blank rows.

All 84 picker tests passed, including grouping, recovery, filtering, selection and inline sign-in cases; the release build passed. An isolated real-PTY run used a private tmux server and an unresolvable explicit SSH destination, verifying the visible gap, the failure below local work, arrow navigation across the divider, clean exit and session preservation. The throwaway capture driver was corrected to wait for the incremental redraw to finish before inspecting the screen. Fixture sessions and capture scripts were removed; no user sessions were changed.

The three-section follow-up also passed all 84 picker tests and a release build. A live picker in an isolated tmux server displayed all three headings and gaps after pinning a session, retained both its shortcut and nested row, and exited without ending the work session. The smoke assertion was scoped to sidebar rows rather than counting the pin-confirmation notice. Fixture servers and the throwaway driver were removed.

All three section headings use the same muted, non-bold style. The focused pinned-section test and release rebuild passed after this refinement; a live tmux capture confirmed that none of the three heading labels had the bold attribute.

Known-offline peers now start in Other computers during the initial scan rather than briefly appearing in Computers. A successful inventory overrides the offline hint and preserves selection when moving the computer up. All 85 picker tests and the release build passed. A live startup capture showed six actual offline Tailscale peers below the divider while the remaining computers were still probing.

PR review corrected the CLI inventory tests to use a fixed hostname and retained the `local` attachment identifier in plain-text headings (`hostname — local`). It also preserved the selected session's identity through a failed probe; the recovery regression now selects the second session and verifies that it returns instead of jumping to the first. Clippy with warnings denied passed, and a fresh Debug executable printed `d3vbook — local` with an isolated, empty tmux directory.

The local full Rust suite passed 276 tests, including all inventory and picker regressions, with one subprocess fixture ignored. One existing test failed: `cancellation_wait_kills_descendant_process_groups_within_half_a_second` exceeded its 500 ms wall-clock assertion. The cancellation implementation and timing test were not changed or retried; passing CI on the final PR head remains the merge gate.

## iPhone terminal keyboard layout (November 20, 2026)

The terminal accessory row now respects bottom and horizontal safe areas, adding space for the home-indicator area when needed. A UIKit keyboard layout guide bounds the terminal above the keyboard and accessory; SwiftUI no longer applies a second keyboard adjustment.

The Debug simulator build passed. On the 402×874-point review simulator, a live SSH/tmux terminal remained connected while switching keyboard modes. With the hardware keyboard connected, the terminal ended at y=796 and the accessory buttons occupied y=800–836, above the 34-point bottom safe area. With the software keyboard visible, the terminal ended at y=522 and the accessory row began there, with no terminal overlap. Screenshots were inspected in both modes; keyboard dismissal was also exercised. No terminal commands were sent or remote sessions ended. Physical-device and landscape checks were not run.

## v0.2.0 release validation (November 17, 2026)

The nonpublishing [Release run 44222032589](https://github.com/delux-sh/delux/actions/runs/44222032589) passed on candidate `83e3a46f36811e35d0416c96ff02c7503e0e309b`: Linux/macOS tests and Clippy, minimum Rust, archive build, and all four native Homebrew package jobs. Each platform installed the generated formula with dependencies, passed `brew test` and `brew audit --strict`, and exercised the installed picker. Platforms were macOS Apple Silicon and Intel, and Linux ARM64 and x86_64; none substituted emulation or skipped dependencies.

All four downloaded archives matched `SHA256SUMS`. Inspection confirmed executable permissions and bundled README, GPL license, generated third-party notices, Rust notices and musl copyright information; staged release notes matched the committed v0.2.0 notes. The local release executable also reported v0.2.0. This validation did not create a tag, publish a GitHub release or update the tap. Tag publication remains gated by another successful release run and protected-environment approval; the tap must use verified published checksums, not these dry-run hashes.

Existing feature-level verification below covers session creation, agent status, add-computer and destructive termination. iPhone distribution is separate: signing-team configuration, distribution signing, an opaque icon export, encryption/privacy/licensing review and physical-device testing are still required before TestFlight readiness.

## Explicit session termination (November 16, 2026)

The CLI's list-only `x` action and iPhone session context/terminal menus open a captured-target confirmation naming the session and computer/SSH destination, with Cancel as the default. The command checks numeric session ID and server lifetime in the same tmux queue as `kill-session`, requires an exact completion receipt, never starts a server or falls back to a name, and never automatically retries an uncertain result. Confirmation does not disconnect the active terminal; successful termination closes only the matching client and invalidates older inventory.

New Rust model and real-tmux regressions passed, covering cancellation, terminal-focused `x`, unseen/clipped confirmations, endpoint changes, stale inventory, unrelated active clients, vanished/replaced servers, name reuse, exact-target termination and the final session's receipt. The live backend cases passed on Homebrew tmux 3.8 and the temporary next-3.9 tmux build. Clippy with warnings denied, Rust 1.88 all-target checking and a release build passed. An existing exact footer-wording assertion failed when the new shortcut changed the hint order; that assertion was removed, and the retained width/completeness layout check passed.

Real picker scenarios passed locally and through loopback OpenSSH: default Enter canceled, Escape canceled, a two-window target ended while another connected shell remained usable, and the last connected session ended with a confirmed result. The throwaway driver's filter-clear key and Escape timing were corrected before the successful scenarios.

All 17 canonical iOS simulator tests passed. A real OpenSSH/tmux UI scenario verified the named confirmation, cancellation, context-menu termination, termination from the connected terminal with return to the dashboard, preservation of another session, and final-session success. The initial smoke selector matched two native accessibility wrappers for Cancel; selecting the first matching button fixed the harness. Screenshots were inspected and retained outside the source tree.

Tests used private tmux sockets, temporary SSH keys/configuration and the owned iPhone simulator. Fixture servers, sessions and smoke harnesses were removed. No real user session was ended. Physical iPhone and physical remote-host testing remain unverified; CI is the merge gate. No release or Homebrew publication is included.

## iPhone companion (November 16, 2026)

A real OMP agent's recognized `touch IOS_AGENT_APPROVED` approval was displayed and accepted through the iPhone terminal. The UI scenario passed; the isolated server contained the marker and OMP displayed `DONE`. All three fixture tmux sessions survived the app's test runs. The fixture SSH server, temporary keys, agent sessions and smoke harness were then removed; screenshots are retained outside the source tree.

The native SwiftUI/SwiftTerm app built with Xcode 26.4.1 and passed all 10 canonical tests on an iPhone 17 Pro simulator running iOS 26.4.1. Tests cover framed tmux data, status priority and expiry, creation identity, rejected input, configuration write failures, cross-computer terminal isolation, and real Keychain credential isolation. Simulator signing remains enabled: an unsigned build correctly failed Keychain access rather than falling back to insecure storage.

An isolated loopback OpenSSH server and real tmux sessions exercised generated-key authentication, exact host-fingerprint verification, session inventory, terminal input (confirmed by a remote marker file), background/foreground reattachment, and creation in a chosen directory. `localhost` resolved to both IPv6 and IPv4 while sshd listened only on IPv4, exercising address fallback. The first lifecycle harness passed through reconnect but failed to locate a menu by an inherited accessibility identifier; the corrected user-facing selector passed the creation scenario separately.

Review fixes include binding authentication to the winning TCP connection rather than every Happy Eyeballs attempt; persisting configuration before changing UI state; rejecting stale trust/refresh results; retaining a trust prompt after a failed write; keeping unrelated terminal connections unchanged during edits; and preventing dismissal while creation is uncertain. No remote helper, fake terminal, mocked SSH backend or production test backdoor is used.

No physical iPhone was connected. Device signing, physical lock/resume, cellular/Tailscale transitions and App Store/TestFlight distribution are not verified. This is a source-build companion, not a published mobile release.

## Reviewed feature integration (November 16, 2026)

Admin-squash merged [#11](https://github.com/delux-sh/delux/pull/11) as `a741d9d`, then [#12](https://github.com/delux-sh/delux/pull/12) as `e3cfc02`, then [#13](https://github.com/delux-sh/delux/pull/13) as `460b0d6`, each after its exact reviewed head passed Linux/macOS CI, Rust 1.88 and DCO checks. The final add-computer branch passed 265 local tests, Clippy, minimum-Rust checking and a release build. Its isolated picker scenario confirmed hidden-duplicate refusal and saving a new computer without replacing existing mappings. No release or Homebrew publication was performed.

## Explicit session creation

Pre-merge November 16: rebased onto agent-status integration `a741d9d`. Clippy, Rust 1.88 all-target checking and the release build passed. The suite passed 255 tests; one new regression assumed discovery order instead of selecting the target by key. After correcting that assumption, the regression passed. Full CI remains the merge gate.

Real-PTY checks observed local creation, chosen directory, command execution, terminal input, and duplicate rejection. The loopback-SSH fixture additionally completed cancel/quit and verified both sessions survived. Fixtures used temporary keys, configuration and tmux sockets; real sessions and SSH settings were untouched. Earlier delayed-response cancellation checks below remain historical; current model regressions cover cancellation and stale scan results.

The remaining historical results in this section cover feature commit `f0ef8e6` on old base `081568b`, before the 0.1.0 release.

The picker creates sessions on a ready local or SSH computer using a name, target directory, and optional shell command. Creation captures the new session's lifetime identity before attachment, rejects duplicate names, and never retries an uncertain mutation. The previous client remains connected while the form is open and after failed or cancelled creation.

Pre-merge review keeps encoding-tool checks in a subshell so their diagnostic `LC_ALL=C` cannot become the new tmux server's locale. Creation submission waits for an ongoing scan, including the uncertainty rescan after failure; successful creation ignores older in-flight inventory for that computer while other computers finish refreshing. These regressions passed in the checks above.

The macOS ARM64 full suite passed: 184 tests, with one subprocess helper ignored in the ordinary run and exercised by its parent. Formatting, Clippy with warnings denied, Rust 1.88 all-target checks, and the release build passed. Five focused creation regressions also passed after final text updates. Tests cover literal shell/tmux metacharacters, duplicate and invalid-directory non-mutation, starting a missing server, target HOME expansion, replacement identity rejection, bounded form input, cancelled completion, and changed computer configuration.

Real-PTY checks exercised the release picker locally and through an isolated loopback OpenSSH server with temporary keys/configuration. Verified chosen working directories, terminal input, duplicate rejection, invalid-directory recovery without starting a server, OMP launch, retained existing sessions, detach/reattach, and session survival after quitting Delux. Side-by-side, stacked, and compact form layouts were observed. Delaying the SSH response after a real creation committed verified that cancellation neither attached nor retried, retained the original client, and allowed the created session to be found after clearing the existing filter.

All fixture servers, agent sessions, keys, and temporary files were removed. This worktree has not run Linux CI or exercised creation on a physical Linux host; OMP startup was checked without submitting a model request. Claude/Codex launch and remote network failure conditions beyond the injected response delay were not separately exercised.

## 0.1.0 release rehearsal (November 15, 2026)

A [nonpublishing Release run](https://github.com/delux-sh/delux/actions/runs/43764792273) passed at `f2817a9`, the last commit before the published-installation wording. It ran Linux and macOS CI and the minimum Rust version check, built all four GPL archives, and installed the generated formula with full dependencies on native macOS Apple Silicon and Intel and native Linux ARM64 and x86_64 runners. Each passed `brew test`, `brew audit --strict`, a byte comparison against its archive, and the real-PTY picker smoke check. The publish job was skipped.

This run covers the selection fix after accepting a filter, the README demo, and the stale-sweep test change. That test failed once on Linux before the change; the cause was inferred from the test rather than reproduced, so a recurrence is possible. Packaged remote SSH, physical terminals, and trackpad behaviour were not exercised.

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

The right panel follows list selection while retaining the connected PTY in the background. Selecting its own session shows the live terminal; selecting another row shows that row's preview. Tab and Enter open the highlighted session, reusing the connected client when it matches. With a non-session row highlighted, Tab still focuses the connected terminal.

All 42 picker tests passed, including a regression covering preview delivery with an active client, hidden PTY output draining, and switching back without reconnecting. Formatting, Clippy with warnings denied, and the native macOS ARM64 release build passed.

An isolated real-PTY run with two local tmux sessions verified selection-driven previews, periodic preview refresh, background output, Tab restoring the exact original client, and Enter attaching the selected session. Browsing did not replace the connected client; quitting preserved both sessions. Fixture processes and files were removed. Remote SSH switching was not separately exercised.

The Tab behavior above supersedes the original connected-terminal shortcut verified in that run. The picker regressions cover opening a different highlighted session and returning to the selected connected session without reconnecting.

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
