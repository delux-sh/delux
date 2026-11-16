use super::*;
use crate::crt::Signal;
use crate::state::{Pin, load_state, update_state};
use crate::test_support::{EnvVar, env_write};
use crate::theme;
use crate::workers::{load_config, local_worker, save_host_mapping};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, KeyCode, KeyModifiers, MouseEventKind};
use ratatui::style::Modifier;
use std::cell::RefCell;
use std::rc::Rc;

const NOW: i64 = 1_000_000;

fn worker(key: &str, name: &str) -> Worker {
    Worker {
        key: key.into(),
        name: name.into(),
        destination: None,
        online: None,
    }
}

fn remote(key: &str, name: &str, destination: &str) -> Worker {
    Worker {
        destination: Some(destination.into()),
        ..worker(key, name)
    }
}

fn session(id: &str, name: &str) -> Session {
    Session {
        id: id.into(),
        name: name.into(),
        windows: 1,
        attached: 0,
        server_pid: 1,
        server_start: 1,
        socket_path: "/tmp/tmux-test".into(),
        created: 1,
        activity: 1,
        output: 1,
        command: String::new(),
        path: String::new(),
        agent: None,
    }
}

fn ready(worker: &Worker, sessions: Vec<Session>) -> Inventory {
    Inventory {
        worker: worker.clone(),
        sessions,
        status: Status::Ready,
        detail: String::new(),
    }
}

fn failed(worker: &Worker, status: Status, detail: &str) -> Inventory {
    Inventory {
        worker: worker.clone(),
        sessions: vec![],
        status,
        detail: detail.into(),
    }
}

/// Observable effects of a picker under test.
#[derive(Default)]
struct Effects {
    launched: Vec<Choice>,
    mappings: Vec<(String, String)>,
    states: Vec<State>,
    previews: Vec<PreviewKey>,
    status_refreshes: Vec<String>,
}

struct Harness {
    picker: Picker,
    effects: Rc<RefCell<Effects>>,
}

impl Harness {
    fn new(workers: Vec<Worker>, inventories: Vec<Inventory>) -> Self {
        Self::with_state(workers, inventories, State::default())
    }

    fn with_state(workers: Vec<Worker>, inventories: Vec<Inventory>, state: State) -> Self {
        Self::with_loaded_state(workers, inventories, Ok(state))
    }

    fn with_loaded_state(
        workers: Vec<Worker>,
        inventories: Vec<Inventory>,
        state: Result<State, String>,
    ) -> Self {
        let effects = Rc::new(RefCell::new(Effects::default()));
        let (mappings, states) = (effects.clone(), effects.clone());
        let mut saved_state = state.as_ref().cloned().unwrap_or_default();
        let services = Services {
            discover: Arc::new(move |_| Ok((workers.clone(), String::new()))),
            scan: Arc::new(move |_, _| {
                let (sender, receiver) = mpsc::channel();
                for inventory in &inventories {
                    sender.send(inventory.clone()).unwrap();
                }
                receiver
            }),
            preview: Arc::new(|_, worker, session| {
                Ok(crate::tmux::Preview {
                    text: format!(
                        "output of {} on {}\n\tindented\x1b[2J",
                        session.name, worker.name
                    ),
                    activity: session.activity,
                    output: session.output,
                })
            }),
            agent_status: Arc::new(|_, _| Ok(Vec::new())),
            resolve_user: Arc::new(|_, _| Err("SSH settings unavailable".into())),
            sharing_active: Box::new(|_| true),
            save_mapping: Box::new(move |name, destination| {
                mappings
                    .borrow_mut()
                    .mappings
                    .push((name.into(), destination.into()));
                Ok(())
            }),
            update_state: Box::new(move |change| {
                saved_state.apply(change);
                states.borrow_mut().states.push(saved_state.clone());
                Ok(saved_state.clone())
            }),
            now: Box::new(|| NOW),
        };
        let mut picker = Picker::new(Cancel::new(), services, state);
        let initial = picker.rescan(true);
        let mut harness = Self { picker, effects };
        harness.drive(Some(initial));
        assert!(!harness.picker.scanning);
        harness
    }

    fn drive(&mut self, command: Option<Command>) {
        let Some(command) = command else {
            return;
        };
        let next = match command {
            Command::Discover { generation, cancel } => {
                let result = (self.picker.services.discover)(&cancel);
                self.picker
                    .update(Message::Discovered { generation, result })
            }
            Command::Scan {
                generation,
                cancel,
                workers,
            } => {
                for inventory in (self.picker.services.scan)(&cancel, workers) {
                    let next = self.picker.update(Message::Inventory {
                        generation,
                        inventory: Some(inventory),
                    });
                    self.drive(next);
                }
                self.picker.update(Message::Inventory {
                    generation,
                    inventory: None,
                })
            }
            Command::AgentStatuses {
                generation,
                cancel,
                workers,
            } => {
                let results = workers
                    .into_iter()
                    .map(|worker| {
                        self.effects
                            .borrow_mut()
                            .status_refreshes
                            .push(worker.key.clone());
                        let result = (self.picker.services.agent_status)(&cancel, &worker);
                        (worker, result)
                    })
                    .collect();
                self.picker.update(Message::AgentStatuses {
                    generation,
                    results,
                })
            }
            Command::Preview {
                worker,
                session,
                cancel,
            } => {
                let key = (worker.key.clone(), session.id.clone());
                self.effects.borrow_mut().previews.push(key.clone());
                let result = (self.picker.services.preview)(&cancel, &worker, &session);
                self.picker.update(Message::Preview {
                    generation: self.picker.generation,
                    destination: worker.destination,
                    key,
                    session,
                    result,
                })
            }
            Command::ResolveUser { worker, cancel } => {
                let result = (self.picker.services.resolve_user)(
                    &cancel,
                    worker.destination.as_deref().unwrap(),
                );
                self.picker.update(Message::SshUser {
                    worker,
                    cancel,
                    result,
                })
            }
            Command::Launch(choice) => {
                self.effects.borrow_mut().launched.push(choice);
                None
            }
        };
        self.drive(next);
    }

    fn press(&mut self, keys: &[&str]) -> bool {
        for key in keys {
            let codes = match *key {
                "enter" => vec![KeyCode::Enter],
                "esc" => vec![KeyCode::Esc],
                "down" => vec![KeyCode::Down],
                "up" => vec![KeyCode::Up],
                "tab" => vec![KeyCode::Tab],
                "backspace" => vec![KeyCode::Backspace],
                text => text.chars().map(KeyCode::Char).collect(),
            };
            for code in codes {
                let next = self
                    .picker
                    .update(Message::Key(KeyEvent::new(code, KeyModifiers::NONE)));
                self.drive(next);
            }
        }
        self.picker.quit
    }

    fn tick(&mut self, at: Instant) {
        let next = self.picker.update(Message::Tick(at));
        self.drive(next);
    }

    fn buffer(&mut self, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| self.picker.render(frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn rendered(&mut self, width: u16, height: u16) -> String {
        let buffer = self.buffer(width, height);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn launched(&self) -> Vec<Choice> {
        self.effects.borrow().launched.clone()
    }
}

/// A real PTY child exercises panel behavior without touching any tmux server.
fn terminal_harness(args: &[&str]) -> Harness {
    let worker = worker("box", "box");
    let session = session("$0", "work");
    let mut harness = Harness::new(
        vec![worker.clone()],
        vec![ready(
            &worker,
            vec![session.clone(), self::session("$1", "other")],
        )],
    );
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let attachment = crate::ssh_mux::begin_attachment();
    let terminal = EmbeddedTerminal::spawn(&args, 24, 80).unwrap();
    harness.picker.active = Some(ActiveTerminal {
        worker,
        session: Some(session),
        started: Instant::now(),
        terminal,
        _attachment: attachment,
    });
    harness.picker.terminal_focused = true;
    harness.buffer(120, 30);
    harness
}

#[test]
fn terminal_focus_keeps_shortcuts_and_paste_out_of_the_picker() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let generation = harness.picker.generation;
    harness.press(&["q", "r", "tab", "/", "1", "2"]);
    assert!(!harness.picker.quit);
    assert!(!harness.picker.filtering);
    assert_eq!(harness.picker.generation, generation);
    assert!(
        harness
            .picker
            .update(Message::Paste("q\nr\t".into()))
            .is_none()
    );
    assert!(!harness.picker.quit);
    assert_eq!(harness.picker.generation, generation);
    assert!(
        harness
            .picker
            .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .is_none()
    );
    assert!(!harness.picker.quit);
    assert!(harness.picker.active.is_some());
}

#[test]
fn directional_focus_preserves_client_and_active_identity_while_browsing() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let back = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert!(harness.picker.key(back).is_none());
    assert!(!harness.picker.terminal_focused);
    assert!(harness.picker.key(back).is_none());
    assert!(!harness.picker.terminal_focused);
    let terminal_columns = (
        harness.picker.terminal_area.x,
        harness.picker.terminal_area.width,
    );
    harness.press(&["/", "other", "enter"]);
    harness.buffer(120, 30);
    assert_eq!(
        (
            harness.picker.terminal_area.x,
            harness.picker.terminal_area.width
        ),
        terminal_columns,
    );
    harness.press(&["esc"]);
    harness.press(&["down", "tab"]);
    assert!(harness.picker.terminal_focused);
    assert_eq!(harness.picker.selected_session().unwrap().1.id, "$1");
    assert_eq!(
        harness
            .picker
            .active
            .as_ref()
            .unwrap()
            .session
            .as_ref()
            .unwrap()
            .id,
        "$0"
    );
    assert!(harness.launched().is_empty());
    assert!(harness.picker.key(back).is_none());
    harness.press(&["1", "2", "tab"]);
    assert!(harness.picker.terminal_focused);
    assert!(harness.launched().is_empty());
    assert!(harness.picker.key(back).is_none());
    harness.press(&["up", "enter"]);
    assert!(harness.launched().is_empty());
    assert!(harness.picker.terminal_focused);
    assert!(harness.picker.key(back).is_none());
    harness.press(&["down", "enter"]);
    let launched = harness.launched();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].session.as_ref().unwrap().id, "$1");
    assert!(!harness.picker.quit);
}

#[test]
fn embedded_panel_stays_visible_when_focused_at_small_sizes() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    for (width, height) in [
        (120, 30),
        (80, 24),
        (60, 24),
        (40, 12),
        (10, 5),
        (2, 2),
        (1, 1),
    ] {
        harness.buffer(width, height);
        assert!(harness.picker.terminal_visible, "{width}x{height}");
        assert!(harness.picker.terminal_area.width <= width);
        assert!(harness.picker.terminal_area.height <= height);
        assert!(!harness.picker.preview_visible);
    }
    harness
        .picker
        .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness.buffer(40, 12);
    assert!(!harness.picker.terminal_visible);
    assert!(harness.picker.list_height > 0);
    assert!(harness.picker.active.is_some());
    harness.press(&["tab"]);
    harness.buffer(40, 12);
    assert!(harness.picker.terminal_visible);
}

#[test]
fn stacked_layout_keeps_the_live_client_size_while_rows_change() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    harness.buffer(70, 40);
    assert!(harness.picker.terminal_visible);
    let area = harness.picker.terminal_area;
    // A refresh keeps the live client's geometry while inventory is pending.
    let Command::Discover { generation, cancel } = harness.picker.rescan(false) else {
        panic!("expected discovery");
    };
    let result = (harness.picker.services.discover)(&cancel);
    let scan = harness
        .picker
        .update(Message::Discovered { generation, result });
    assert!(matches!(scan, Some(Command::Scan { .. })));
    harness.buffer(70, 40);
    assert_eq!(harness.picker.terminal_area, area);
}

#[test]
fn browsing_previews_other_sessions_without_disconnect() {
    let mut harness = terminal_harness(&[
        "/bin/sh",
        "-c",
        "printf 'CONNECTED_OUTPUT\\r\\n'; read line; printf 'BACKGROUND_OUTPUT\\r\\n'; exec /bin/cat",
    ]);
    let started = harness.picker.active.as_ref().unwrap().started;
    let cancel = Cancel::new();
    harness.picker.preview_inflight = Some((("box".into(), "$0".into()), cancel.clone()));
    let now = Instant::now();
    harness.tick(now);
    assert!(cancel.is_cancelled());
    let back = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
    harness.picker.key(back);
    harness.buffer(120, 30);
    assert!(harness.picker.terminal_visible);
    assert!(!harness.picker.preview_visible);

    harness.press(&["down"]);
    harness.buffer(120, 30);
    assert!(!harness.picker.terminal_visible);
    assert!(harness.picker.preview_visible);
    harness.tick(now);
    harness.tick(now + PREVIEW_DEBOUNCE);
    let view = harness.rendered(120, 30);
    assert!(view.contains("output of other on box"), "{view}");

    // The hidden PTY must still be drained, not disconnected or paused.
    harness
        .picker
        .active
        .as_mut()
        .unwrap()
        .terminal
        .paste("\n")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(harness.picker.poll_terminal().is_none());
        if harness
            .picker
            .active
            .as_ref()
            .unwrap()
            .terminal
            .contents()
            .contains("BACKGROUND_OUTPUT")
        {
            break;
        }
        assert!(Instant::now() < deadline, "hidden PTY stopped draining");
        std::thread::sleep(Duration::from_millis(5));
    }

    harness.press(&["tab"]);
    let view = harness.rendered(120, 30);
    assert!(view.contains("BACKGROUND_OUTPUT"), "{view}");
    assert!(!view.contains("output of other on box"), "{view}");
    assert!(harness.picker.terminal_visible);
    assert!(!harness.picker.preview_visible);
    assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
    assert_eq!(harness.picker.selected_session().unwrap().1.id, "$1");
    assert!(harness.launched().is_empty());

    harness.picker.key(back);
    let view = harness.rendered(120, 30);
    assert!(view.contains("output of other on box"), "{view}");
    harness.press(&["up"]);
    let view = harness.rendered(120, 30);
    assert!(view.contains("BACKGROUND_OUTPUT"), "{view}");
    assert!(harness.picker.terminal_visible);
}

#[test]
fn child_exit_returns_to_list_and_rescans_before_resuming_previews() {
    let mut harness = terminal_harness(&["/bin/sh", "-c", "exit 7"]);
    let generation = harness.picker.generation;
    let deadline = Instant::now() + Duration::from_secs(5);
    let command = loop {
        if let Some(command) = harness.picker.poll_terminal() {
            break command;
        }
        assert!(Instant::now() < deadline, "PTY child did not exit");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(matches!(command, Command::Discover { .. }));
    assert!(harness.picker.active.is_none());
    assert!(!harness.picker.terminal_focused);
    assert!(!harness.picker.quit);
    assert_eq!(harness.picker.generation, generation + 1);
    harness.drive(Some(command));
    harness.buffer(120, 30);
    let now = Instant::now();
    harness.tick(now);
    harness.tick(now + PREVIEW_DEBOUNCE);
    assert_eq!(harness.effects.borrow().previews.len(), 1);
}

fn sign_in_harness() -> Harness {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let active = harness.picker.active.as_mut().unwrap();
    active.session = None;
    active.worker.destination = Some("sign-in.example".into());
    let worker = active.worker.clone();
    harness.picker.entries[0] = Entry {
        inventory: Some(failed(&worker, Status::AuthRequired, "Permission denied")),
        worker,
    };
    harness.picker.selection = Selection::Computer("box".into());
    harness.picker.rebuild();
    harness
}

#[test]
fn sign_in_refocus_and_cancellation_preserve_the_picker() {
    let mut harness = sign_in_harness();
    let started = harness.picker.active.as_ref().unwrap().started;
    harness
        .picker
        .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness.press(&["enter"]);
    assert!(harness.picker.terminal_focused);
    assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
    assert!(harness.launched().is_empty());
    harness
        .picker
        .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(harness.picker.active.is_none());
    assert!(!harness.picker.terminal_focused && !harness.picker.quit);
    assert_eq!(harness.picker.entries[0].status(), Status::AuthRequired);
    let command = harness.picker.activate();
    harness.drive(command);
    assert!(harness.picker.input.as_ref().unwrap().connect);
}

#[test]
fn resolved_ssh_user_preserves_alias_and_settings_until_explicitly_changed() {
    let worker = remote("box", "box", "ssh-alias");
    for override_user in [false, true] {
        let mut harness = Harness::new(
            vec![worker.clone()],
            vec![failed(&worker, Status::AuthRequired, "Permission denied")],
        );
        harness.picker.services.resolve_user = Arc::new(|_, _| Ok("configured-user".into()));
        harness.press(&["enter"]);
        assert_eq!(
            harness.picker.input.as_ref().unwrap().text,
            "configured-user"
        );
        if override_user {
            harness
                .picker
                .key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
            harness.press(&["different-user"]);
        } else {
            harness.picker.services.save_mapping =
                Box::new(|_, _| panic!("confirming SSH settings must not rewrite configuration"));
        }
        harness.press(&["enter"]);
        let launched = harness.launched();
        if override_user {
            assert_eq!(
                launched[0].worker.destination.as_deref(),
                Some("different-user@ssh-alias")
            );
            assert_eq!(
                harness.effects.borrow().mappings,
                vec![("box".into(), "different-user@ssh-alias".into())],
            );
        } else {
            assert_eq!(launched[0].worker, worker);
        }
    }
}

#[test]
fn username_lookup_cannot_overwrite_edits_or_a_reopened_form() {
    let worker = remote("box", "box", "ssh-alias");
    let mut harness = Harness::new(
        vec![worker.clone()],
        vec![failed(&worker, Status::AuthRequired, "Permission denied")],
    );
    let Some(Command::ResolveUser { cancel, .. }) = harness.picker.activate() else {
        panic!("expected SSH settings lookup");
    };
    harness.press(&["enter"]);
    assert!(harness.launched().is_empty());
    harness.picker.update(Message::Paste("typed-user".into()));
    assert!(cancel.is_cancelled());
    harness.picker.update(Message::SshUser {
        worker: worker.clone(),
        cancel,
        result: Ok("late-user".into()),
    });
    assert_eq!(harness.picker.input.as_ref().unwrap().text, "typed-user");

    harness.press(&["esc"]);
    let Some(Command::ResolveUser { cancel: old, .. }) = harness.picker.activate() else {
        panic!("expected SSH settings lookup");
    };
    harness.press(&["esc"]);
    assert!(old.is_cancelled());
    let Some(Command::ResolveUser {
        cancel: current, ..
    }) = harness.picker.activate()
    else {
        panic!("expected new SSH settings lookup");
    };
    harness.picker.update(Message::SshUser {
        worker: worker.clone(),
        cancel: old,
        result: Ok("stale-user".into()),
    });
    assert!(harness.picker.input.as_ref().unwrap().text.is_empty());
    harness.press(&["enter"]);
    harness.picker.update(Message::SshUser {
        worker,
        cancel: current,
        result: Ok("current-user".into()),
    });
    assert_eq!(harness.picker.input.as_ref().unwrap().text, "current-user");
    assert!(harness.picker.notice.is_empty());
    harness.press(&["enter"]);
    assert_eq!(
        harness.launched()[0].worker.destination.as_deref(),
        Some("ssh-alias")
    );
}

#[test]
fn failed_ssh_lookup_requires_a_remote_username_before_launch_and_reconciles_results() {
    let worker = remote("node-1", "box", "box.example");
    let mut harness = Harness::new(
        vec![worker.clone()],
        vec![failed(&worker, Status::AuthRequired, "Permission denied")],
    );
    harness.press(&["enter"]);
    assert!(harness.picker.input.as_ref().unwrap().text.is_empty());
    harness.press(&["enter"]);
    assert!(harness.launched().is_empty());
    assert!(harness.effects.borrow().mappings.is_empty());
    harness.press(&["remote-user", "enter"]);
    let launched = harness.launched();
    assert_eq!(
        launched[0].worker.destination.as_deref(),
        Some("remote-user@box.example")
    );
    assert!(launched[0].session.is_none());
    assert_eq!(
        harness.effects.borrow().mappings,
        vec![("node-1".into(), "remote-user@box.example".into())],
    );
    harness.picker.update(Message::Checked(ready(
        &launched[0].worker,
        vec![session("$1", "project")],
    )));
    assert_eq!(harness.picker.selected_session().unwrap().1.id, "$1");
}

#[test]
fn sign_in_confirms_an_explicit_user_without_requiring_a_config_write() {
    let worker = remote("box", "box", "remote-user@box.example");
    let mut harness = Harness::new(
        vec![worker.clone()],
        vec![failed(&worker, Status::HostTrust, "Unknown host key")],
    );
    harness.picker.services.save_mapping = Box::new(|_, _| Err("read-only config".into()));
    harness.press(&["enter"]);
    assert_eq!(harness.picker.input.as_ref().unwrap().text, "remote-user");
    harness.press(&["enter"]);
    assert_eq!(
        harness.launched(),
        vec![Choice {
            worker,
            session: None
        }]
    );
}

#[test]
fn sign_in_changes_an_explicit_user_and_blocks_unsaved_or_stale_overrides() {
    let worker = remote("box", "box", "wrong@box.example");
    let mut harness = Harness::new(
        vec![worker.clone()],
        vec![failed(&worker, Status::AuthRequired, "Permission denied")],
    );
    harness.press(&["enter"]);
    harness
        .picker
        .key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    harness.press(&["right"]);
    let save = std::mem::replace(
        &mut harness.picker.services.save_mapping,
        Box::new(|_, _| Err("read-only config".into())),
    );
    harness.press(&["enter"]);
    assert!(harness.launched().is_empty());
    assert_eq!(harness.picker.input.as_ref().unwrap().text, "right");
    assert_eq!(harness.picker.entries[0].worker, worker);
    harness.picker.services.save_mapping = save;
    harness.picker.entries[0].worker.destination = Some("changed.example".into());
    harness.press(&["enter"]);
    assert!(harness.effects.borrow().mappings.is_empty());
    assert!(harness.launched().is_empty());
    harness.picker.entries[0].worker = worker;
    harness.press(&["enter"]);
    assert_eq!(
        harness.launched()[0].worker.destination.as_deref(),
        Some("right@box.example"),
    );
}

#[test]
fn sign_in_form_cancels_without_replacing_the_connected_client() {
    let mut harness = sign_in_harness();
    harness.picker.active.as_mut().unwrap().session = Some(session("$0", "work"));
    let started = harness.picker.active.as_ref().unwrap().started;
    harness
        .picker
        .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness.press(&["enter", "another-user"]);
    for (width, height) in [(120, 30), (70, 40), (40, 12), (2, 2)] {
        harness.buffer(width, height);
        assert!(!harness.picker.terminal_visible);
        assert!(!harness.picker.preview_visible);
        assert!(harness.picker.terminal_area.width > 0);
    }
    harness
        .picker
        .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(harness.picker.input.is_none());
    assert!(!harness.picker.quit);
    assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
    assert!(harness.launched().is_empty());
    assert!(harness.effects.borrow().mappings.is_empty());
}

#[test]
fn sign_in_timeout_returns_to_a_usable_list() {
    let mut harness = sign_in_harness();
    harness.picker.active.as_mut().unwrap().started = Instant::now() - Duration::from_secs(121);
    assert!(harness.picker.poll_terminal().is_none());
    assert!(harness.picker.active.is_none());
    assert!(!harness.picker.terminal_focused && !harness.picker.quit);
    assert_eq!(harness.picker.entries[0].status(), Status::ProbeFailed);
}

#[test]
fn completed_sign_in_cannot_populate_a_changed_destination() {
    let original = remote("box", "box", "old.example");
    let mut harness = Harness::new(
        vec![original.clone()],
        vec![failed(&original, Status::AuthRequired, "Permission denied")],
    );
    harness.picker.entries[0].worker.destination = Some("new.example".into());
    let next = harness.picker.update(Message::Checked(ready(
        &original,
        vec![session("$1", "old-destination-session")],
    )));
    assert!(matches!(next, Some(Command::Discover { .. })));
    assert!(harness.picker.entries[0].sessions().is_empty());
    assert_eq!(
        harness.picker.entries[0].worker.destination.as_deref(),
        Some("new.example")
    );
}

#[test]
fn pasted_sidebar_text_never_executes_shortcuts_or_control_characters() {
    let mut harness = Harness::new(vec![], vec![]);
    let generation = harness.picker.generation;
    assert!(
        harness
            .picker
            .update(Message::Paste("q\nr\t".into()))
            .is_none()
    );
    assert!(!harness.picker.quit);
    assert_eq!(harness.picker.generation, generation);
    harness.press(&["/"]);
    harness.picker.update(Message::Paste("q\nr\t\u{1b}".into()));
    assert_eq!(harness.picker.filter, "qr");
    assert!(harness.picker.filtering);
}

#[test]
fn filter_selects_exact_computer_and_session() {
    let workers = vec![worker("a", "host-a"), worker("b", "host-b")];
    let inventories = vec![
        ready(&workers[0], vec![session("$0", "work")]),
        ready(
            &workers[1],
            vec![session("$1", "work"), session("$2", "Straße")],
        ),
    ];
    let scenarios: Vec<(&str, Vec<&str>, &str)> = vec![
        ("computer name", vec!["/", "HOST-B", "enter", "enter"], "$1"),
        (
            "case-folded session",
            vec!["/", "strasse", "enter", "enter"],
            "$2",
        ),
        (
            "cleared filter",
            vec!["/", "nope", "esc", "down", "down", "enter"],
            "$2",
        ),
        (
            "edited filter",
            vec!["/", "host-bx", "backspace", "down", "enter", "enter"],
            "$2",
        ),
    ];
    for (name, keys, expected) in scenarios {
        let mut harness = Harness::new(workers.clone(), inventories.clone());
        harness.press(&keys);
        let launched = harness.launched();
        assert_eq!(launched.len(), 1, "{name}");
        assert_eq!(launched[0].worker.key, "b", "{name}");
        assert_eq!(launched[0].session.as_ref().unwrap().id, expected, "{name}");
    }
}

#[test]
fn clearing_a_filter_keeps_an_accepted_row_and_cancelling_restores_the_earlier_one() {
    let workers = vec![worker("a", "host-a"), worker("b", "host-b")];
    let inventories = vec![
        ready(&workers[0], vec![session("$0", "work")]),
        ready(
            &workers[1],
            vec![session("$1", "other"), session("$2", "Straße")],
        ),
    ];
    let scenarios: Vec<(&str, Vec<&str>, &str)> = vec![
        ("cancelled while typing", vec!["/", "strasse", "esc"], "$0"),
        ("accepted", vec!["/", "strasse", "enter", "esc"], "$2"),
        (
            "accepted and opened",
            vec!["/", "strasse", "enter", "enter", "esc"],
            "$2",
        ),
    ];
    for (name, keys, expected) in scenarios {
        let mut harness = Harness::new(workers.clone(), inventories.clone());
        harness.press(&keys);
        assert!(harness.picker.filter.is_empty(), "{name}");
        assert_eq!(
            harness.picker.selected_session().unwrap().1.id,
            expected,
            "{name}"
        );
    }
}

#[test]
fn unified_list_includes_sessions_and_computers_without_sessions() {
    let up = worker("up", "has-work");
    let empty = remote("empty", "no-work", "empty.tail");
    let auth = remote("auth", "needs-key", "auth.tail");
    let mut offline = remote("off", "gone", "off.tail");
    offline.online = Some(false);
    let mut harness = Harness::new(
        vec![up.clone(), empty.clone(), auth.clone(), offline.clone()],
        vec![
            ready(&up, vec![session("$0", "work")]),
            ready(&empty, vec![]),
            failed(&auth, Status::AuthRequired, "Permission denied"),
            failed(&offline, Status::Unreachable, "no route"),
        ],
    );
    let view = harness.rendered(100, 20);
    assert!(view.contains("has-work") && view.contains("work"), "{view}");
    for visible in ["no-work", "needs-key", "gone"] {
        assert!(view.contains(visible), "{visible} missing:\n{view}");
    }
}

#[test]
fn ssh_banners_follow_the_error_they_precede() {
    // OpenSSH 10 prints these before the real failure.
    let detail = "** WARNING: connection is not using a post-quantum key exchange algorithm.\n\
                      ** This session may be vulnerable to \"store now, decrypt later\" attacks.\n\
                      alice@example-host: Permission denied (publickey).";
    assert_eq!(
        first_line(detail),
        "alice@example-host: Permission denied (publickey)."
    );
    assert_eq!(first_line("** only a banner"), "** only a banner");
    let board = remote("example", "example-host", "alice@example-host");
    let mut harness = Harness::new(
        vec![board.clone()],
        vec![failed(&board, Status::AuthRequired, detail)],
    );
    let view = harness.rendered(160, 30);
    let error = view.find("Permission denied").expect("error shown");
    let banner = view.find("post-quantum").expect("banner kept");
    assert!(error < banner, "error first:\n{view}");
}

#[test]
fn unified_list_orders_computers_and_authenticates_inline() {
    let up = worker("up", "has-work");
    let empty = remote("empty", "no-work", "empty.tail");
    let auth = remote("auth", "needs-key", "auth.tail");
    let mut offline = remote("off", "gone", "off.tail");
    offline.online = Some(false);
    let mut harness = Harness::new(
        vec![up.clone(), empty.clone(), auth.clone(), offline.clone()],
        vec![
            ready(&up, vec![session("$0", "work")]),
            ready(&empty, vec![]),
            failed(
                &auth,
                Status::AuthRequired,
                "alice@auth: Permission denied (publickey).",
            ),
            failed(&offline, Status::Unreachable, "no route"),
        ],
    );
    let view = harness.rendered(100, 20);
    let order: Vec<usize> = ["has-work", "needs-key", "no-work", "gone"]
        .iter()
        .map(|name| view.find(name).unwrap_or_else(|| panic!("{name}:\n{view}")))
        .collect();
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{view}");
    harness.press(&["down"]);
    let view = harness.rendered(160, 30);
    assert!(
        view.contains("Permission denied") && view.contains("Press u"),
        "{view}"
    );
    assert!(view.contains("NO SIGNAL"), "{view}");
    // Enter asks for the remote username before connecting interactively.
    harness.press(&["enter", "alice", "enter"]);
    let launched = harness.launched();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].worker.key, "auth");
    assert!(launched[0].session.is_none());
    // Enter on a computer without sessions explains instead of launching.
    harness.press(&["down", "enter"]);
    assert_eq!(harness.launched().len(), 1);
    assert!(
        harness.picker.notice.contains("no tmux sessions"),
        "{}",
        harness.picker.notice
    );
    // Completion expands the computer without stealing selection while browsing.
    harness.picker.update(Message::Checked(ready(
        &launched[0].worker,
        vec![session("$9", "new-work")],
    )));
    assert_eq!(
        harness.picker.selection,
        Selection::Computer("empty".into())
    );
    harness.press(&["up", "enter"]);
    let launched = harness.launched();
    assert_eq!(launched[1].worker.key, "auth");
    assert_eq!(launched[1].session.as_ref().unwrap().id, "$9");
}

#[test]
fn offline_computers_render_dim_hollow_status_lights() {
    let mut offline = remote("off", "offline-host", "offline.example.net");
    offline.online = Some(false);
    let inventory = failed(&offline, Status::Unreachable, "no route");
    let mut harness = Harness::new(vec![offline], vec![inventory]);
    // Inspect the semantic status style without the selection highlight.
    harness.picker.cursor = None;
    let buffer = harness.buffer(100, 20);
    let light = buffer
        .content
        .iter()
        .find(|cell| cell.symbol() == "○")
        .expect("offline computer has a hollow status light");
    assert!(light.modifier.contains(Modifier::DIM));
    assert_ne!(Some(light.fg), theme::error().fg);
}

#[test]
fn rescan_expands_selected_computer_and_ignores_stale_results() {
    let worker = remote("dev", "dev", "alice@dev");
    let inventory = failed(
        &worker,
        Status::AuthRequired,
        "alice@dev: Permission denied (publickey).\nsecond diagnostic",
    );
    let mut harness = Harness::new(vec![worker.clone()], vec![inventory]);
    harness.press(&["enter", "enter"]);
    let launched = harness.launched();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].worker.key, "dev");
    assert!(launched[0].session.is_none());
    let superseded = harness.picker.generation;
    harness.press(&["r"]);
    let picker = &mut harness.picker;
    assert_ne!(picker.generation, superseded);
    assert!(picker.entries[0].inventory.is_some());
    let ready = ready(&worker, vec![session("$9", "late")]);
    picker.entries[0].inventory = None;
    picker.pending.insert(worker.key.clone());
    picker.update(Message::Inventory {
        generation: superseded,
        inventory: Some(ready.clone()),
    });
    assert!(picker.entries[0].inventory.is_none());
    picker.update(Message::Discovered {
        generation: superseded,
        result: Err("stale error".into()),
    });
    assert!(!picker.quit);
    // A current scan result expands the selected computer in place.
    picker.update(Message::Inventory {
        generation: picker.generation,
        inventory: Some(ready.clone()),
    });
    assert_eq!(picker.entries[0].inventory.as_ref(), Some(&ready));
    assert_eq!(
        picker.selection,
        Selection::Session {
            worker: "dev".into(),
            session: "$9".into(),
            pinned: false
        }
    );
    assert!(harness.press(&["q"]));
    assert_eq!(harness.picker.code, 0);
}

#[test]
fn selection_survives_later_results() {
    let workers = vec![worker("a", "first"), worker("b", "second")];
    let inventories = vec![
        ready(&workers[1], vec![session("$1", "kept")]),
        ready(&workers[0], vec![session("$0", "inserted")]),
    ];
    let mut harness = Harness::new(workers, inventories);
    harness.press(&["enter"]);
    let launched = harness.launched();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].worker.key, "b");
    assert_eq!(launched[0].session.as_ref().unwrap().id, "$1");
}

#[test]
fn refresh_detach_and_filter_keep_order_until_explicit_rescan() {
    fn assert_order(harness: &mut Harness, names: &[&str]) {
        let view = harness.rendered(60, 24);
        let positions: Vec<_> = names
            .iter()
            .map(|name| view.find(name).unwrap_or_else(|| panic!("{name}: {view}")))
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{view}");
    }

    let alpha = worker("a", "alpha");
    let beta = worker("b", "beta");
    let gamma = remote("c", "gamma", "gamma.example");
    let delta = remote("d", "delta", "delta.example");
    let mut first = session("$1", "first");
    first.activity = 20;
    let mut second = session("$2", "second");
    second.activity = 10;
    let mut harness = Harness::new(
        vec![alpha.clone(), beta.clone(), gamma.clone()],
        vec![
            ready(
                &alpha,
                vec![second.clone(), first.clone(), session("$3", "removed")],
            ),
            ready(&beta, vec![]),
            failed(&gamma, Status::AuthRequired, "Sign in"),
        ],
    );
    assert_order(
        &mut harness,
        &["alpha", "first", "second", "removed", "gamma", "beta"],
    );
    harness.press(&["down"]);
    let selection = harness.picker.selection.clone();
    // Preview activity updates must not reshuffle a subsequent row rebuild.
    harness.picker.update(Message::Preview {
        generation: harness.picker.generation,
        destination: None,
        key: ("a".into(), "$2".into()),
        session: second.clone(),
        result: Ok(crate::tmux::Preview {
            text: "new output".into(),
            activity: 40,
            output: 40,
        }),
    });
    harness.press(&["/", "second", "esc"]);
    assert_order(
        &mut harness,
        &["first", "second", "removed", "gamma", "beta"],
    );
    assert_eq!(harness.picker.selection, selection);
    // Status changes expand a computer in place instead of promoting it.
    let mut beta_older = session("$0", "beta-older");
    beta_older.activity = 10;
    let mut beta_newer = session("$1", "beta-newer");
    beta_newer.activity = 99;
    harness
        .picker
        .update(Message::Checked(ready(&beta, vec![beta_older, beta_newer])));
    assert_order(
        &mut harness,
        &["alpha", "gamma", "beta", "beta-newer", "beta-older"],
    );
    assert_eq!(harness.picker.selection, selection);

    second.activity = 40;
    let mut added = session("$4", "added");
    added.activity = 30;
    let mut newest = session("$5", "newest");
    newest.activity = 50;
    let workers = vec![beta.clone(), delta.clone(), alpha.clone()];
    let inventories = vec![
        failed(&delta, Status::AuthRequired, "Sign in"),
        ready(&beta, vec![]),
        ready(&alpha, vec![added, newest, second, first]),
    ];
    harness.picker.services.discover = Arc::new(move |_| Ok((workers.clone(), String::new())));
    harness.picker.services.scan = Arc::new(move |_, _| {
        let (sender, receiver) = mpsc::channel();
        for inventory in &inventories {
            sender.send(inventory.clone()).unwrap();
        }
        receiver
    });
    // Detach drives the same discovery/scan path as an automatic refresh.
    let next = harness
        .picker
        .update(Message::Attached(Ok(AttachOutcome::Exited(0))));
    harness.drive(next);
    assert_order(
        &mut harness,
        &[
            "alpha", "first", "second", "newest", "added", "beta", "delta",
        ],
    );
    let view = harness.rendered(60, 24);
    assert!(
        !view.contains("gamma") && !view.contains("removed"),
        "{view}"
    );
    assert_eq!(harness.picker.selection, selection);
    harness.press(&["r"]);
    assert_order(
        &mut harness,
        &[
            "alpha", "newest", "second", "added", "first", "delta", "beta",
        ],
    );
    assert_eq!(harness.picker.selection, selection);
}

#[test]
fn sixty_column_picker_uses_concise_rows_and_complete_priority_hints() {
    let computer = remote("a", "studio", "a-very-long-ssh-destination.example");
    let mut work = session("$0", "project");
    work.output = NOW - 120;
    work.command = "long-command-that-must-not-be-chopped".into();
    work.path = "/a/long/working/directory".into();
    let mut harness = Harness::new(vec![computer.clone()], vec![ready(&computer, vec![work])]);
    let view = harness.rendered(60, 24);
    assert!(view.contains("project    2m"), "{view}");
    assert!(
        !view.contains("long-command") && !view.contains("a-very-long"),
        "{view}"
    );
    assert!(!view.contains("CH "), "{view}");
    assert_eq!(
        view.lines().last().unwrap(),
        " enter/tab open  q quit  / filter  f pin  r rescan  ↑↓ move"
    );
    // At every width, the widget receives complete hints, not clipped spans.
    for width in 1..=80 {
        let line = harness.picker.footer_line(width);
        assert!(line.width() <= usize::from(width));
        assert!(line.spans.len().is_multiple_of(3));
    }
    harness.press(&["/", "proj"]);
    let view = harness.rendered(60, 24);
    assert_eq!(
        view.lines().last().unwrap(),
        " type to filter  ↑↓ move  enter done  esc clear"
    );
}

#[test]
fn sixty_column_focus_keeps_terminal_visible() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let view = harness.rendered(60, 24);
    assert!(harness.picker.terminal_visible && harness.picker.list_area.is_empty());
    assert!(view.lines().last().unwrap().contains("shift-tab"));
}

#[test]
fn sixty_column_sign_in_keeps_its_hints_and_layouts() {
    let mut harness = sign_in_harness();
    let view = harness.rendered(60, 24);
    assert!(harness.picker.terminal_visible);
    let footer = view.lines().last().unwrap();
    assert!(footer.contains("shift-tab") && footer.contains("ctrl-c"));
    harness
        .picker
        .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness.picker.active = None;
    harness.press(&["enter"]);
    let view = harness.rendered(60, 24);
    assert!(!harness.picker.terminal_visible && harness.picker.list_area.is_empty());
    assert!(
        view.contains("Remote username:") && view.contains("Esc    Back"),
        "{view}"
    );
    let footer = view.lines().last().unwrap();
    assert!(footer.contains("ctrl-u") && footer.ends_with("esc back"));
}

#[test]
fn local_session_requests_attachment_even_inside_another_server() {
    let _env = env_write();
    let _tmux = EnvVar::set("TMUX", "/tmp/another-server,1,0");
    let worker = local_worker();
    let inventory = ready(&worker, vec![session("$0", "work")]);
    let mut harness = Harness::new(vec![worker], vec![inventory]);
    harness.press(&["enter"]);
    let launched = harness.launched();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].session.as_ref().unwrap().id, "$0");
    assert!(!harness.picker.quit);
}

#[test]
fn escapes_remote_control_sequences() {
    let mut worker = worker("x", "name\x1b[2J");
    worker.destination = Some("evil\x1b]0;title\x07host".into());
    let inventory = failed(&worker, Status::AuthRequired, "detail\x1b[5;7m\nmore\x07");
    let mut harness = Harness::new(vec![worker], vec![inventory]);
    for (width, height) in [(180, 30), (90, 30)] {
        let view = harness.rendered(width, height);
        assert!(!view.contains('\x1b'), "{view}");
        assert!(!view.contains('\x07'), "{view}");
        assert!(view.contains(r"name\x1b[2J"), "{view}");
    }
}

#[test]
fn discovery_failure_and_interrupt_set_exit_codes() {
    let mut harness = Harness::new(vec![], vec![]);
    let picker = &mut harness.picker;
    picker.update(Message::Discovered {
        generation: picker.generation,
        result: Err("discovery failed".into()),
    });
    assert!(picker.quit);
    assert_eq!(picker.code, 1);
    assert_eq!(picker.error.as_deref(), Some("discovery failed"));
    let mut harness = Harness::new(vec![], vec![]);
    harness.picker.update(Message::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert!(harness.picker.quit);
    assert_eq!(harness.picker.code, exit_codes::INTERRUPTED);
}

#[test]
fn every_exit_cancels_picker_work_without_cancelling_the_session() {
    for exit in ["quit", "interrupt", "discovery error", "drop"] {
        let parent = Cancel::new();
        let mut harness = Harness::new(vec![], vec![]);
        harness.picker.cancel = parent.child();
        let _ = harness.picker.rescan(false);
        let scan = harness.picker.scan_cancel.as_ref().unwrap().clone();
        let preview = harness.picker.cancel.child();
        harness.picker.preview_inflight = Some((("host".into(), "$0".into()), preview.clone()));
        match exit {
            "quit" => {
                harness.press(&["q"]);
            }
            "interrupt" => {
                harness.picker.update(Message::Key(KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL,
                )));
            }
            "discovery error" => {
                harness.picker.update(Message::Discovered {
                    generation: harness.picker.generation,
                    result: Err("discovery failed".into()),
                });
            }
            _ => {}
        }
        drop(harness);
        assert!(scan.is_cancelled(), "{exit}: scan");
        assert!(preview.is_cancelled(), "{exit}: preview");
        assert!(!parent.is_cancelled(), "{exit}: parent session");
    }
}

#[test]
fn attaching_rescans_and_reports_failure_without_creating_sessions() {
    let mut harness = Harness::new(vec![], vec![]);
    let generation = harness.picker.generation;
    let next = harness
        .picker
        .update(Message::Attached(Ok(AttachOutcome::Exited(7))));
    harness.drive(next);
    assert_eq!(harness.picker.generation, generation + 1);
    assert_eq!(
        harness.picker.notice,
        "Attachment ended with status 7; no replacement session was created."
    );
    let next = harness
        .picker
        .update(Message::Attached(Err("failure\x1b".into())));
    harness.drive(next);
    assert_eq!(harness.picker.notice, r"Attach failed: failure\x1b");
}

#[test]
fn preview_waits_for_a_resting_selection_and_tracks_it() {
    let workers = vec![worker("a", "box")];
    let mut older = session("$1", "older");
    older.activity = NOW - 600;
    let mut newer = session("$2", "newer");
    newer.activity = NOW - 30;
    let mut harness = Harness::new(
        workers.clone(),
        vec![ready(&workers[0], vec![older, newer])],
    );
    harness.rendered(180, 30);
    let start = Instant::now();
    harness.tick(start);
    harness.tick(start + Duration::from_millis(100));
    assert!(
        harness.effects.borrow().previews.is_empty(),
        "fetched before resting"
    );
    harness.tick(start + PREVIEW_DEBOUNCE);
    // Most recently active session is listed and selected first.
    assert_eq!(
        harness.effects.borrow().previews,
        vec![("a".into(), "$2".into())]
    );
    let view = harness.rendered(180, 30);
    assert!(view.contains("output of newer on box"), "{view}");
    assert!(view.contains("        indented"), "tabs expand:\n{view}");
    assert!(!view.contains('\x1b'));
    // Fresh previews are not refetched; stale ones are.
    harness.tick(start + PREVIEW_DEBOUNCE + PREVIEW_REFRESH / 2);
    assert_eq!(harness.effects.borrow().previews.len(), 1);
    harness.tick(start + PREVIEW_DEBOUNCE + PREVIEW_REFRESH);
    assert_eq!(harness.effects.borrow().previews.len(), 2);
    // A result for a session that is no longer selected is cached but not shown.
    harness.press(&["down"]);
    let later = start + Duration::from_secs(30);
    harness.tick(later);
    // Changing selection shows static first, then the tuning screen.
    let view = harness.rendered(180, 30);
    assert!(view.contains('▒') || view.contains('▓'), "static:\n{view}");
    harness.tick(later + SELECTION_STATIC);
    let next = harness.picker.update(Message::Preview {
        generation: harness.picker.generation,
        destination: workers[0].destination.clone(),
        key: ("a".into(), "$2".into()),
        session: harness.picker.entries[0]
            .sessions()
            .iter()
            .find(|s| s.id == "$2")
            .unwrap()
            .clone(),
        result: Ok(crate::tmux::Preview {
            text: "late result for newer".into(),
            activity: NOW,
            output: NOW,
        }),
    });
    harness.drive(next);
    let view = harness.rendered(180, 30);
    assert!(!view.contains("late result"), "{view}");
    assert!(view.contains("TUNING"), "{view}");
    // No preview pane, no fetches.
    harness.rendered(60, 12);
    harness.tick(later + PREVIEW_REFRESH * 2);
    assert_eq!(harness.effects.borrow().previews.len(), 2);
}

#[test]
fn preview_updates_live_ages_only_for_the_same_session_identity() {
    let computer = worker("a", "box");
    let mut original = session("$1", "printing");
    original.activity = NOW - 120;
    original.output = NOW - 70;
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![original.clone()])],
    );
    let before = harness.rendered(180, 30);
    assert!(before.contains("printing    1m"), "{before}");
    harness.picker.update(Message::Preview {
        generation: harness.picker.generation,
        destination: computer.destination.clone(),
        key: ("a".into(), "$1".into()),
        session: original.clone(),
        result: Ok(crate::tmux::Preview {
            text: "live output".into(),
            activity: NOW - 10,
            output: NOW,
        }),
    });
    let after = harness.rendered(180, 30);
    assert!(after.contains("printing   now"), "{after}");
    assert!(after.contains("output now · used 10s ago"), "{after}");
    let current = &harness.picker.entries[0].sessions()[0];
    assert_eq!((current.activity, current.output), (NOW - 10, NOW));
    for change in [
        (|s: &mut Session| s.created += 1) as fn(&mut Session),
        |s| s.server_pid += 1,
        |s| s.server_start += 1,
        |s| s.socket_path.push('x'),
    ] {
        let mut stale = original.clone();
        change(&mut stale);
        harness.picker.update(Message::Preview {
            generation: harness.picker.generation,
            destination: computer.destination.clone(),
            key: ("a".into(), "$1".into()),
            session: stale,
            result: Ok(crate::tmux::Preview {
                text: "wrong lifetime".into(),
                activity: 1,
                output: 1,
            }),
        });
        assert_eq!(harness.picker.entries[0].sessions()[0].output, NOW);
        assert!(!harness.rendered(180, 30).contains("wrong lifetime"));
    }
}

#[test]
fn preview_access_failure_offers_sign_in_and_recovers_sessions() {
    for status in [Status::AuthRequired, Status::HostTrust] {
        let computer = remote("a", "box", "alice@box");
        let other = worker("b", "local");
        let selected = session("$1", "project");
        let mut harness = Harness::new(
            vec![computer.clone(), other.clone()],
            vec![
                ready(&computer, vec![selected.clone()]),
                ready(&other, vec![session("$2", "unrelated")]),
            ],
        );
        harness.picker.show_session("a", "$1");
        harness.rendered(120, 30);
        let start = Instant::now();
        harness.tick(start);
        harness.tick(start + PREVIEW_DEBOUNCE);
        assert!(harness.rendered(120, 30).contains("output of project"));
        harness.picker.services.preview =
            Arc::new(move |_, _, _| Err((status, "SSH access rejected".into())));
        harness.tick(start + PREVIEW_DEBOUNCE + PREVIEW_REFRESH);
        assert_eq!(harness.picker.entries[0].status(), status);
        assert_eq!(harness.picker.selection, Selection::Computer("a".into()));
        assert_eq!(harness.picker.entries[1].sessions()[0].name, "unrelated");
        let view = harness.rendered(120, 30);
        assert!(
            view.contains(match status {
                Status::AuthRequired => "Sign in",
                _ => "Trust key",
            }),
            "{view}"
        );
        assert!(!view.contains("output of project"), "{view}");
        harness.press(&["enter", "enter"]);
        assert_eq!(
            harness.launched(),
            vec![Choice {
                worker: computer.clone(),
                session: None,
            }]
        );
        harness
            .picker
            .update(Message::Checked(ready(&computer, vec![selected])));
        assert_eq!(
            harness.picker.selection,
            Selection::Session {
                worker: "a".into(),
                session: "$1".into(),
                pinned: false,
            }
        );
        assert_eq!(harness.picker.screen().signal, Signal::Tuning);
    }
}

#[test]
fn preview_access_failure_ignores_stale_results_and_local_permissions() {
    for boundary in ["scan", "destination", "session", "local"] {
        let computer = if boundary == "local" {
            worker("a", "box")
        } else {
            remote("a", "box", "alice@box")
        };
        let selected = session("$1", "project");
        let mut harness = Harness::new(
            vec![computer.clone()],
            vec![ready(&computer, vec![selected.clone()])],
        );
        harness.rendered(120, 30);
        let start = Instant::now();
        harness.picker.tick(start);
        let Some(Command::Preview { cancel, .. }) = harness.picker.tick(start + PREVIEW_DEBOUNCE)
        else {
            panic!("preview not scheduled");
        };
        let generation = harness.picker.generation;
        let mut previous_session = selected.clone();
        match boundary {
            "scan" => {
                harness.picker.rescan(false);
                assert!(cancel.is_cancelled());
                assert!(harness.picker.preview_inflight.is_none());
            }
            "destination" => {
                harness.picker.entries[0].worker.destination = Some("bob@box".into());
            }
            "session" => previous_session.server_start += 1,
            _ => {}
        }
        harness.picker.update(Message::Preview {
            generation,
            destination: computer.destination,
            key: ("a".into(), "$1".into()),
            session: previous_session,
            result: Err((Status::AuthRequired, "Permission denied".into())),
        });
        assert_eq!(
            harness.picker.entries[0].status(),
            Status::Ready,
            "{boundary}"
        );
        assert_eq!(
            harness.picker.entries[0].sessions(),
            &[selected],
            "{boundary}"
        );
    }
}

#[test]
fn replacement_session_reusing_a_tmux_id_never_shows_cached_output() {
    let computer = worker("a", "box");
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![session("$1", "old")])],
    );
    harness.rendered(180, 30);
    let start = Instant::now();
    harness.tick(start);
    harness.tick(start + PREVIEW_DEBOUNCE);
    assert!(harness.rendered(180, 30).contains("output of old on box"));
    // The tmux server restarted, and its new session reuses the ID.
    let mut replacement = session("$1", "new");
    replacement.server_pid += 1;
    harness.picker.entries[0].inventory = Some(ready(&computer, vec![replacement]));
    harness.picker.rebuild();
    assert_eq!(harness.picker.screen().signal, Signal::Tuning);
    assert!(!harness.rendered(180, 30).contains("output of old"));
    // It is fetched without waiting for the old capture to go stale.
    harness.tick(start + PREVIEW_DEBOUNCE + PREVIEW_REFRESH / 2);
    assert_eq!(harness.effects.borrow().previews.len(), 2);
    assert!(harness.rendered(180, 30).contains("output of new on box"));
}

#[test]
fn preview_refresh_respects_connection_sharing_and_failure_backoff() {
    for local in [false, true] {
        for shared in [false, true] {
            for succeeded in [false, true] {
                let computer = if local {
                    worker("a", "box")
                } else {
                    remote("a", "box", "box.example")
                };
                let mut harness = Harness::new(
                    vec![computer.clone()],
                    vec![ready(&computer, vec![session("$1", "main")])],
                );
                harness.picker.services.sharing_active = Box::new(move |_| shared);
                if !succeeded {
                    harness.picker.services.preview =
                        Arc::new(|_, _, _| Err((Status::ProbeFailed, "failed".into())));
                }
                harness.rendered(180, 30);
                let start = Instant::now();
                harness.tick(start);
                let fetched = start + PREVIEW_DEBOUNCE;
                harness.tick(fetched);
                assert_eq!(harness.effects.borrow().previews.len(), 1);
                let seconds = if succeeded && (local || shared) { 1 } else { 5 };
                harness.tick(fetched + Duration::from_millis(seconds * 1000 - 1));
                assert_eq!(harness.effects.borrow().previews.len(), 1);
                harness.tick(fetched + Duration::from_secs(seconds));
                assert_eq!(harness.effects.borrow().previews.len(), 2);
            }
        }
    }
}

#[test]
fn moving_away_cancels_an_inflight_preview() {
    let workers = vec![worker("a", "box")];
    let mut harness = Harness::new(
        workers.clone(),
        vec![ready(
            &workers[0],
            vec![session("$1", "one"), session("$2", "two")],
        )],
    );
    harness.rendered(180, 30);
    let start = Instant::now();
    harness.picker.update(Message::Tick(start));
    let Some(Command::Preview { cancel, .. }) = harness
        .picker
        .update(Message::Tick(start + PREVIEW_DEBOUNCE))
    else {
        panic!("expected a preview fetch");
    };
    harness.press(&["down"]);
    harness
        .picker
        .update(Message::Tick(start + PREVIEW_DEBOUNCE * 2));
    assert!(cancel.is_cancelled());
}

#[test]
fn setting_the_ssh_user_saves_a_mapping_and_rescans() {
    let worker = remote("node-1", "example-host", "example-host.example.net");
    let inventory = failed(
        &worker,
        Status::AuthRequired,
        "bob@example-host: Permission denied",
    );
    let mut harness = Harness::new(vec![worker], vec![inventory]);
    let generation = harness.picker.generation;
    harness.press(&["u", "bad user", "enter"]);
    assert!(harness.effects.borrow().mappings.is_empty());
    assert!(!harness.picker.notice.is_empty());
    assert_eq!(harness.picker.generation, generation);
    harness
        .picker
        .key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    harness.press(&["alice"]);
    harness.press(&["enter"]);
    assert_eq!(
        harness.effects.borrow().mappings,
        vec![("node-1".into(), "alice@example-host.example.net".into())]
    );
    assert_eq!(harness.picker.generation, generation + 1);
    harness.press(&["u", "x", "esc"]);
    assert_eq!(harness.effects.borrow().mappings.len(), 1);
    assert!(harness.picker.input.is_none());
}

#[test]
fn ssh_user_edits_override_discovery_mappings_by_stable_key() {
    for (id, shared, mapping) in [
        ("node-1", false, "node-1"),
        ("", false, "box.example.test"),
        ("shared-node", true, "box.example.test"),
    ] {
        let root = crate::test_support::TempDir::new();
        let path = root.path().join("cli.json");
        let path = path.to_str().unwrap();
        save_host_mapping(path, mapping, "old-host").unwrap();
        let raw = serde_json::json!({
            "BackendState": "Running",
            "Self": {"UserID": 42},
            "Peer": {"peer": {
                "ID": id,
                "UserID": 42,
                "DNSName": "box.example.test.",
                "HostName": "box",
                "OS": "linux",
                "ShareeNode": shared,
            }},
        })
        .to_string();
        let worker = crate::workers::peer_workers(&raw, &load_config(path).unwrap())
            .unwrap()
            .remove(0);
        let key = worker.key.clone();
        let inventory = failed(&worker, Status::AuthRequired, "Permission denied");
        let mut harness = Harness::new(vec![worker], vec![inventory]);
        let saved_path = path.to_owned();
        harness.picker.services.save_mapping =
            Box::new(move |key, destination| save_host_mapping(&saved_path, key, destination));
        harness.press(&["u", "alice", "enter"]);

        let config = load_config(path).unwrap();
        assert_eq!(config.hosts[&key], "alice@old-host");
        assert!(!config.hosts.contains_key("box"));
        let discovered = crate::workers::peer_workers(&raw, &config).unwrap();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].key, key);
        assert_eq!(discovered[0].destination.as_deref(), Some("alice@old-host"));
    }
}

#[test]
fn stale_pickers_merge_operations_and_adopt_saved_state() {
    let root = crate::test_support::TempDir::new();
    let path = root.path().join("state.json");
    let path = path.to_str().unwrap();
    let mut pickers: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|key| {
            let worker = worker(key, key);
            let inventory = ready(&worker, vec![session("$0", "main")]);
            let mut harness =
                Harness::with_loaded_state(vec![worker], vec![inventory], load_state(path));
            let saved_path = path.to_owned();
            harness.picker.services.update_state =
                Box::new(move |change| update_state(&saved_path, change));
            harness
        })
        .collect();
    pickers[0].press(&["f", "enter"]);
    pickers[1].press(&["f", "enter"]);
    assert!(pickers[1].picker.state.is_pinned("a", "main"));
    assert!(pickers[1].picker.state.is_pinned("b", "main"));
    pickers[0].press(&["f"]);
    let saved = load_state(path).unwrap();
    assert!(!saved.is_pinned("a", "main"));
    assert!(saved.is_pinned("b", "main"));
    assert!(saved.recent.is_empty());
    assert_eq!(pickers[0].picker.state, saved);
}

#[test]
fn only_explicit_pins_are_shown_and_saved() {
    let workers = vec![worker("a", "alpha"), worker("b", "beta")];
    let inventories = vec![
        ready(&workers[0], vec![session("$0", "main")]),
        ready(
            &workers[1],
            vec![session("$1", "work"), session("$2", "side")],
        ),
    ];
    let state = State {
        pins: vec![Pin {
            worker: "b".into(),
            session: "work".into(),
        }],
        recent: vec![crate::state::Recent {
            worker: "a".into(),
            session: "main".into(),
        }],
    };
    let mut harness = Harness::with_state(workers, inventories, state);
    let view = harness.rendered(180, 30);
    assert!(view.contains("Pinned") && view.contains("★ work"), "{view}");
    assert!(!view.contains("★ main") && !view.contains('↺'), "{view}");
    // Pinned rows lead the list; one attaches by name to the live session.
    harness.press(&["g", "enter"]);
    let launched = harness.launched();
    assert_eq!(launched[0].worker.key, "b");
    assert_eq!(launched[0].session.as_ref().unwrap().id, "$1");
    assert!(
        harness.effects.borrow().states.is_empty(),
        "attaching does not save history"
    );
    // Pinning another session saves it; it then appears in the pinned section.
    harness.press(&["G", "f"]);
    let saved = harness.effects.borrow().states.last().cloned().unwrap();
    assert!(saved.is_pinned("b", "side"), "{saved:?}");
    assert_eq!(saved.recent, harness.picker.state.recent);
    assert_eq!(saved.recent[0].worker, "a");
    assert!(harness.rendered(180, 30).contains("★ side"));
    // A pin whose session is gone is not offered.
    harness.picker.state.toggle_pin("a", "missing");
    harness.picker.rebuild();
    assert!(!harness.rendered(180, 30).contains("missing"));
}

#[test]
fn launching_does_not_add_pins_and_keeps_the_session_selected() {
    let computer = worker("a", "box");
    let mut harness = Harness::with_state(
        vec![computer.clone()],
        vec![ready(
            &computer,
            vec![session("$0", "main"), session("$1", "side")],
        )],
        State {
            pins: vec![],
            recent: vec![crate::state::Recent {
                worker: "a".into(),
                session: "side".into(),
            }],
        },
    );
    assert!(!harness.rendered(60, 24).contains("Pinned"));
    harness.press(&["down", "enter"]);
    let launched = harness.launched();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].session.as_ref().unwrap().id, "$1");
    let picker = &harness.picker;
    assert!(picker.rows.iter().all(|row| row.kind != RowKind::Pinned));
    assert!(harness.effects.borrow().states.is_empty());
    assert!(
        picker
            .current_row()
            .is_some_and(|row| row.kind == RowKind::Session)
    );
    assert_eq!(picker.selected_session().unwrap().1.id, "$1");
    assert!(!harness.rendered(60, 24).contains("Pinned"));
    harness.press(&["f"]);
    assert!(harness.rendered(60, 24).contains("Pinned"));
    harness.press(&["f"]);
    assert!(!harness.rendered(60, 24).contains("Pinned"));
}

#[test]
fn pinned_rows_keep_their_computer_visible_in_the_sidebar() {
    let workers = vec![worker("a", "studio"), worker("b", "laptop")];
    let long = "a-really-long-session-name";
    let inventories = vec![
        ready(&workers[0], vec![session("$0", "main")]),
        ready(
            &workers[1],
            vec![session("$1", "main"), session("$2", long)],
        ),
    ];
    let pin = |worker: &str, session: &str| Pin {
        worker: worker.into(),
        session: session.into(),
    };
    let state = State {
        pins: vec![pin("a", "main"), pin("b", "main"), pin("b", long)],
        ..State::default()
    };
    let mut harness = Harness::with_state(workers, inventories, state);
    let buffer = harness.buffer(100, 20);
    // The sidebar's text inside its borders, one entry per row.
    let sidebar: Vec<String> = (0..20u16)
        .map(|y| {
            (1..31u16)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect();
    let header = sidebar
        .iter()
        .position(|line| line.contains("Pinned"))
        .expect("pinned section");
    let pinned = &sidebar[header + 1..header + 4];
    for (line, host) in pinned.iter().zip(["studio", "laptop", "laptop"]) {
        assert!(line.ends_with(host), "{host}: {sidebar:#?}");
    }
    assert!(pinned[2].contains("a-really-l") && pinned[2].contains('…'));
}

#[test]
fn state_save_failures_are_reported() {
    let workers = vec![worker("a", "alpha")];
    let mut harness = Harness::new(
        workers.clone(),
        vec![ready(&workers[0], vec![session("$0", "main")])],
    );
    harness.picker.services.update_state = Box::new(|_| Err("disk full".into()));
    harness.press(&["f"]);
    assert!(harness.rendered(180, 30).contains("disk full"));
}

#[test]
fn unreadable_state_is_reported_and_never_overwritten_by_picker_actions() {
    let root = crate::test_support::TempDir::new();
    let path = root.path().join("state.json");
    let raw = r#"{"favorites":[{"computer":"old","session":"saved"}]}"#;
    std::fs::write(&path, raw).unwrap();
    let path = path.to_str().unwrap();
    let loaded = load_state(path);
    let error = loaded.as_ref().unwrap_err();
    assert!(error.starts_with(path));
    assert!(error.contains("move it aside to reset pins"));
    let computer = worker("a", "box");
    let mut harness = Harness::with_loaded_state(
        vec![computer.clone()],
        vec![ready(&computer, vec![session("$1", "main")])],
        loaded,
    );
    let saved_path = path.to_owned();
    harness.picker.services.update_state =
        Box::new(move |change| update_state(&saved_path, change));
    harness.press(&["f", "enter"]);
    assert!(harness.picker.state.is_pinned("a", "main"));
    assert!(harness.picker.state.recent.is_empty());
    assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
    assert!(
        harness
            .rendered(240, 30)
            .contains("unreadable picker state")
    );
    assert!(!harness.picker.state_writable);
}

fn wheel(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn wheel_coordinates_follow_rendered_terminal_content() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    for (width, height) in [(120, 30), (70, 40), (40, 12), (2, 2), (1, 1)] {
        harness.buffer(width, height);
        let area = harness.picker.terminal_area;
        for kind in [
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            for (column, row) in [(0, 0), (area.width - 1, area.height - 1)] {
                let mut event = wheel(kind, area.x + column, area.y + row);
                event.modifiers = KeyModifiers::SHIFT | KeyModifiers::CONTROL;
                assert_eq!(
                    harness.picker.terminal_mouse_event(event),
                    Some(MouseEvent {
                        column,
                        row,
                        ..event
                    }),
                    "{width}x{height}",
                );
            }
        }
    }
}

#[test]
fn focused_terminal_wheel_reaches_the_child_at_the_translated_cell() {
    let mut harness = terminal_harness(&[
        "/bin/sh",
        "-c",
        "stty raw -echo; printf '\\033[?1000h\\033[?1006hWHEEL_READY'; \
             dd bs=1 count=10 2>/dev/null | od -An -tx1; sleep 30",
    ]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(harness.picker.poll_terminal().is_none());
        if harness.rendered(120, 30).contains("WHEEL_READY") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "PTY mouse reporting did not start"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let area = harness.picker.terminal_area;
    assert!(
        harness
            .picker
            .update(Message::Mouse(wheel(
                MouseEventKind::ScrollDown,
                area.x + 2,
                area.y + 3,
            )))
            .is_none()
    );
    loop {
        assert!(harness.picker.poll_terminal().is_none());
        if harness
            .rendered(120, 30)
            .split_whitespace()
            .collect::<String>()
            .contains("1b5b3c36353b333b344d")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child did not receive the SGR wheel"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(harness.picker.notice.is_empty());
}

#[test]
fn wheel_ignores_borders_other_panes_and_nonwheel_events() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let area = harness.picker.terminal_area;
    let list = harness.picker.list_area;
    let selection = harness.picker.selection.clone();
    for (column, row) in [
        (area.x - 1, area.y),
        (area.x, area.y - 1),
        (area.right(), area.y),
        (area.x, area.bottom()),
        (list.x, list.y),
        (0, 0),
    ] {
        let event = wheel(MouseEventKind::ScrollDown, column, row);
        assert!(harness.picker.terminal_mouse_event(event).is_none());
        assert!(harness.picker.update(Message::Mouse(event)).is_none());
    }
    for kind in [
        MouseEventKind::Down(event::MouseButton::Left),
        MouseEventKind::Up(event::MouseButton::Left),
        MouseEventKind::Drag(event::MouseButton::Left),
        MouseEventKind::Moved,
    ] {
        let event = wheel(kind, area.x, area.y);
        assert!(harness.picker.terminal_mouse_event(event).is_none());
        assert!(harness.picker.update(Message::Mouse(event)).is_none());
    }
    assert_eq!(harness.picker.selection, selection);
    assert!(harness.picker.notice.is_empty());
    assert!(harness.picker.terminal_focused);
}

#[test]
fn sidebar_wheel_uses_rendered_bounds_and_explicit_focus() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    for (width, height) in [(120, 30), (70, 40), (40, 12)] {
        harness
            .picker
            .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        harness.buffer(width, height);
        let list = harness.picker.list_area;
        let initial = harness.picker.selection.clone();
        let down = wheel(MouseEventKind::ScrollDown, list.right() - 1, list.y);
        assert!(harness.picker.update(Message::Mouse(down)).is_none());
        assert_ne!(harness.picker.selection, initial);
        let up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..down
        };
        assert!(harness.picker.update(Message::Mouse(up)).is_none());
        assert_eq!(harness.picker.selection, initial);
        for event in [
            wheel(MouseEventKind::ScrollDown, list.x - 1, list.y),
            wheel(MouseEventKind::ScrollDown, list.x, list.y - 1),
            wheel(MouseEventKind::ScrollRight, list.x, list.y),
        ] {
            assert!(harness.picker.update(Message::Mouse(event)).is_none());
            assert_eq!(harness.picker.selection, initial);
        }
        if harness.picker.terminal_visible {
            let area = harness.picker.terminal_area;
            let before = harness.rendered(width, height);
            let event = wheel(MouseEventKind::ScrollDown, area.x, area.y);
            assert!(harness.picker.terminal_mouse_event(event).is_none());
            assert!(harness.picker.update(Message::Mouse(event)).is_none());
            assert_eq!(harness.picker.selection, initial);
            assert!(harness.picker.notice.is_empty());
            assert!(harness.picker.show_scroll_hint);
            assert!(!harness.picker.terminal_focused);
            let view = harness.rendered(width, height);
            let middle = usize::from(area.y + area.height / 2);
            assert!(
                view.lines()
                    .skip(middle.saturating_sub(1))
                    .take(3)
                    .any(|line| line.contains("Tab"))
            );
            assert_eq!(view.lines().last(), before.lines().last());
            assert_eq!(harness.picker.terminal_area, area);
            assert!(harness.launched().is_empty());
        }
        harness.press(&["tab"]);
        assert!(!harness.picker.show_scroll_hint);
        assert!(harness.picker.update(Message::Mouse(down)).is_none());
        assert_eq!(harness.picker.selection, initial);
    }
}

#[test]
fn preview_wheel_guidance_does_not_attach_or_focus_a_background_client() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    harness
        .picker
        .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness.press(&["down"]);
    for connected in [true, false] {
        if !connected {
            harness.picker.active = None;
        }
        for (width, height) in [(120, 30), (70, 40)] {
            harness.buffer(width, height);
            assert!(harness.picker.preview_visible);
            let selection = harness.picker.selection.clone();
            let area = harness.picker.terminal_area;
            harness.picker.notice = "Existing diagnostic".into();
            let before = harness.rendered(width, height);
            let event = wheel(MouseEventKind::ScrollDown, area.x, area.y);
            assert!(harness.picker.update(Message::Mouse(event)).is_none());
            assert!(harness.picker.show_scroll_hint);
            assert_eq!(harness.picker.selection, selection);
            assert!(!harness.picker.terminal_focused);
            assert_eq!(harness.picker.active.is_some(), connected);
            assert!(harness.launched().is_empty());
            let view = harness.rendered(width, height);
            let area = harness.picker.terminal_area;
            let middle = usize::from(area.y + area.height / 2);
            assert!(
                view.lines()
                    .skip(middle.saturating_sub(1))
                    .take(3)
                    .any(|line| line.contains("Enter"))
            );
            assert_eq!(view.lines().last(), before.lines().last());
            assert!(view.contains("Existing diagnostic"));
            harness.press(&["esc"]);
            assert!(!harness.picker.show_scroll_hint);
            for kind in [MouseEventKind::Moved, MouseEventKind::ScrollDown] {
                let mut outside = wheel(kind, area.x - 1, area.y);
                assert!(harness.picker.update(Message::Mouse(outside)).is_none());
                assert!(!harness.picker.show_scroll_hint);
                outside.column = area.x;
                harness.picker.filtering = true;
                assert!(harness.picker.update(Message::Mouse(outside)).is_none());
                assert!(!harness.picker.show_scroll_hint);
                harness.picker.filtering = false;
            }
        }
    }
}

#[test]
fn wheel_requires_visible_active_terminal_without_dialogs() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let area = harness.picker.terminal_area;
    let event = wheel(MouseEventKind::ScrollDown, area.x, area.y);
    harness.picker.filtering = true;
    assert!(harness.picker.terminal_mouse_event(event).is_none());
    assert!(harness.picker.update(Message::Mouse(event)).is_none());
    harness.picker.filtering = false;
    harness.picker.input = Some(UserInput {
        key: "box".into(),
        name: "box".into(),
        destination: "box".into(),
        text: String::new(),
        connect: false,
        ssh_user: None,
        lookup: None,
    });
    assert!(harness.picker.terminal_mouse_event(event).is_none());
    assert!(harness.picker.update(Message::Mouse(event)).is_none());
    harness.picker.input = None;
    harness
        .picker
        .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness.buffer(40, 12);
    harness.press(&["tab"]);
    assert!(!harness.picker.terminal_visible);
    assert!(harness.picker.terminal_mouse_event(event).is_none());
    assert!(harness.picker.update(Message::Mouse(event)).is_none());
    assert!(harness.picker.notice.is_empty());
    harness.buffer(40, 12);
    assert_eq!(harness.picker.list_area, Rect::default());
    let area = harness.picker.terminal_area;
    let event = wheel(MouseEventKind::ScrollDown, area.x, area.y);
    assert!(harness.picker.terminal_mouse_event(event).is_some());
    assert!(harness.picker.update(Message::Mouse(event)).is_none());
    assert!(harness.picker.notice.contains("tmux prefix+["));
    assert!(harness.picker.notice.contains("set -g mouse on"));
    harness.picker.notice.clear();
    harness.picker.active = None;
    assert!(harness.picker.terminal_mouse_event(event).is_none());
    assert!(harness.picker.update(Message::Mouse(event)).is_none());
    assert!(harness.picker.notice.is_empty());
}

fn with_agent(mut session: Session, state: AgentState, since: Option<i64>) -> Session {
    session.agent = Some(crate::tmux::AgentStatus { state, since });
    session
}

#[test]
fn agent_markers_keep_columns_and_waiting_header_in_both_layouts() {
    let computer = worker("box", "box");
    let sessions = vec![
        with_agent(
            session("$1", "waiting"),
            AgentState::Waiting,
            Some(NOW - 180),
        ),
        with_agent(session("$2", "working"), AgentState::Working, Some(NOW)),
        with_agent(session("$3", "done"), AgentState::Done, Some(NOW - 7200)),
        with_agent(session("$5", "error"), AgentState::Error, None),
        session("$4", "quiet"),
    ];
    let mut harness = Harness::new(vec![computer.clone()], vec![ready(&computer, sessions)]);
    for width in [70, 120] {
        let buffer = harness.buffer(width, 30);
        let lines: Vec<String> = (0..30)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(lines[0].contains(" · 1 waiting"));
        let mut name_columns = Vec::new();
        let mut age_columns = Vec::new();
        for (name, marker, style) in [
            (
                "waiting",
                "●",
                theme::warning().add_modifier(Modifier::BOLD),
            ),
            ("working", "●", harness.picker.tint.secondary()),
            ("done", "✓", theme::success()),
            ("error", "✗", theme::error()),
            ("quiet", " ", harness.picker.tint.secondary()),
        ] {
            let y = lines
                .iter()
                .position(|line| line.contains(name) && line.contains("11d"))
                .unwrap();
            let cells: Vec<_> = (0..width).map(|x| buffer[(x, y as u16)].symbol()).collect();
            let x = cells
                .windows(name.len())
                .position(|cells| cells.concat() == name)
                .unwrap();
            name_columns.push(x);
            age_columns.push(
                cells
                    .windows(3)
                    .position(|cells| cells.concat() == "11d")
                    .unwrap(),
            );
            let marker_cell = &buffer[((x - 3) as u16, y as u16)];
            assert_eq!(marker_cell.symbol(), marker);
            assert_eq!(marker_cell.fg, style.fg.unwrap());
            if name == "waiting" {
                assert!(marker_cell.modifier.contains(Modifier::BOLD));
            }
        }
        assert!(name_columns.windows(2).all(|pair| pair[0] == pair[1]));
        assert!(age_columns.windows(2).all(|pair| pair[0] == pair[1]));
    }
    let start = Instant::now();
    harness.tick(start);
    harness.tick(start + PREVIEW_DEBOUNCE);
    assert!(
        harness
            .picker
            .screen()
            .detail
            .starts_with("agent waiting 3m")
    );
    harness.press(&["down"]);
    assert!(
        harness
            .picker
            .screen()
            .detail
            .starts_with("agent working now")
    );
    harness.press(&["down"]);
    assert!(harness.picker.screen().detail.starts_with("agent done 2h"));
    harness.press(&["down"]);
    assert!(harness.picker.screen().detail.starts_with("agent error · "));
    harness.picker.terminal_focused = true;
    let buffer = harness.buffer(120, 30);
    for (state, expected) in [
        (AgentState::Waiting, theme::warning()),
        (AgentState::Error, theme::error()),
        (AgentState::Working, theme::inactive_sidebar()),
        (AgentState::Done, theme::inactive_sidebar()),
    ] {
        let index = harness
            .picker
            .rows
            .iter()
            .position(|row| {
                harness
                    .picker
                    .session(*row)
                    .is_some_and(|session| session.agent.is_some_and(|agent| agent.state == state))
            })
            .unwrap();
        let cell = &buffer[(
            harness.picker.list_area.x + 2,
            harness.picker.list_area.y + index as u16,
        )];
        assert_eq!(cell.fg, expected.fg.unwrap());
    }
}

#[test]
fn working_marker_expires_only_without_recent_status_or_output() {
    let computer = worker("box", "box");
    for (state, since, output, visible) in [
        (AgentState::Working, None, NOW - 601, false),
        (AgentState::Working, None, NOW - 600, true),
        (AgentState::Working, Some(NOW - 601), NOW - 601, false),
        (AgentState::Working, Some(NOW - 600), 1, true),
        (AgentState::Working, Some(1), NOW, true),
        (AgentState::Waiting, Some(1), 1, true),
        (AgentState::Error, Some(1), 1, true),
        (AgentState::Error, None, 1, true),
        (AgentState::Done, None, 1, true),
    ] {
        let mut session = with_agent(session("$1", "work"), state, since);
        session.output = output;
        let mut harness = Harness::new(
            vec![computer.clone()],
            vec![ready(&computer, vec![session])],
        );
        let view = harness.rendered(70, 20);
        assert_eq!(
            view.contains(match state {
                AgentState::Done => "✓",
                AgentState::Error => "✗",
                AgentState::Working | AgentState::Waiting => "●",
            }),
            visible
        );
        let start = Instant::now();
        harness.tick(start);
        harness.tick(start + PREVIEW_DEBOUNCE);
        assert_eq!(
            harness.picker.screen().detail.starts_with("agent "),
            visible
        );
    }
}

#[test]
fn periodic_agent_refresh_changes_marker_without_touching_order_selection_or_preview() {
    let computer = worker("box", "box");
    let mut first = session("$1", "first");
    first.activity = NOW;
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![first, session("$2", "second")])],
    );
    harness.picker.services.agent_status = Arc::new(|_, _| {
        Ok(vec![AgentStatusRow {
            server_pid: 1,
            server_start: 1,
            session_id: "$2".into(),
            agent: Some(crate::tmux::AgentStatus {
                state: AgentState::Waiting,
                since: None,
            }),
        }])
    });
    harness.rendered(120, 30);
    let start = Instant::now();
    harness.tick(start);
    harness.tick(start + PREVIEW_DEBOUNCE);
    let selection = harness.picker.selection.clone();
    harness.picker.notice = "keep this notice".into();
    // The selection's due preview goes first; the status refresh takes the
    // next tick and leaves previews alone.
    harness.tick(start + STATUS_REFRESH);
    assert!(harness.effects.borrow().status_refreshes.is_empty());
    let previews = harness.effects.borrow().previews.clone();
    harness.tick(start + STATUS_REFRESH + Duration::from_millis(16));
    assert_eq!(harness.effects.borrow().status_refreshes, ["box"]);
    assert_eq!(harness.effects.borrow().previews, previews);
    assert_eq!(harness.picker.selection, selection);
    assert_eq!(harness.picker.notice, "keep this notice");
    let sessions = harness.picker.entries[0].sessions();
    assert_eq!(
        sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        ["$1", "$2"]
    );
    assert_eq!(sessions[1].agent.unwrap().state, AgentState::Waiting);
    let view = harness.rendered(120, 30);
    assert!(view.contains("●  second"));
    assert!(view.contains("1 waiting"));
    harness.picker.update(Message::AgentStatuses {
        generation: harness.picker.generation,
        results: vec![(
            computer,
            Ok(vec![AgentStatusRow {
                server_pid: 1,
                server_start: 1,
                session_id: "$2".into(),
                agent: Some(crate::tmux::AgentStatus {
                    state: AgentState::Error,
                    since: None,
                }),
            }]),
        )],
    });
    assert_eq!(harness.picker.entries[0].sessions()[0].id, "$1");
    assert_eq!(harness.picker.selection, selection);
    assert_eq!(harness.effects.borrow().previews, previews);
    let view = harness.rendered(120, 30);
    assert!(view.contains("✗  second"));
    assert!(!view.contains("1 waiting"));
}

#[test]
fn agent_refresh_ignores_old_generation_other_server_and_failures() {
    let computer = worker("box", "box");
    let original = with_agent(session("$1", "work"), AgentState::Done, None);
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![original.clone()])],
    );
    let generation = harness.picker.generation;
    for (generation, pid, start) in [
        (generation - 1, 1, 1),
        (generation, 2, 1),
        (generation, 1, 2),
    ] {
        harness.picker.update(Message::AgentStatuses {
            generation,
            results: vec![(
                computer.clone(),
                Ok(vec![AgentStatusRow {
                    server_pid: pid,
                    server_start: start,
                    session_id: "$1".into(),
                    agent: Some(crate::tmux::AgentStatus {
                        state: AgentState::Waiting,
                        since: None,
                    }),
                }]),
            )],
        });
        assert_eq!(
            harness.picker.entries[0].sessions(),
            std::slice::from_ref(&original)
        );
    }
    harness.picker.update(Message::AgentStatuses {
        generation,
        results: vec![(computer, Err("untrusted failure".into()))],
    });
    assert_eq!(harness.picker.entries[0].sessions(), &[original]);
    assert!(harness.picker.notice.is_empty());
}

#[test]
fn agent_refresh_requires_ready_sessions_and_shared_remote_connection() {
    let local = worker("local", "local");
    let shared = remote("shared", "shared", "shared.example");
    let unshared = remote("unshared", "unshared", "unshared.example");
    let empty = worker("empty", "empty");
    let failed = worker("failed", "failed");
    let mut harness = Harness::new(
        vec![
            local.clone(),
            shared.clone(),
            unshared.clone(),
            empty.clone(),
            failed.clone(),
        ],
        vec![
            ready(&local, vec![session("$1", "local")]),
            ready(&shared, vec![session("$1", "shared")]),
            ready(&unshared, vec![session("$1", "unshared")]),
            ready(&empty, vec![]),
            self::failed(&failed, Status::ProbeFailed, "failed"),
        ],
    );
    harness.picker.services.sharing_active =
        Box::new(|destination| destination == "shared.example");
    let start = Instant::now();
    harness.tick(start);
    assert!(harness.effects.borrow().status_refreshes.is_empty());
    let command = harness.picker.update(Message::Tick(start + STATUS_REFRESH));
    assert!(matches!(command, Some(Command::AgentStatuses { .. })));
    assert!(
        harness
            .picker
            .update(Message::Tick(start + STATUS_REFRESH * 2))
            .is_none()
    );
    harness.drive(command);
    assert_eq!(
        harness.effects.borrow().status_refreshes,
        ["local", "shared"]
    );
    harness.picker.scanning = true;
    harness.tick(start + STATUS_REFRESH * 3);
    assert_eq!(harness.effects.borrow().status_refreshes.len(), 2);
    harness.picker.scanning = false;
    harness.picker.quit();
    harness.tick(start + STATUS_REFRESH * 4);
    assert_eq!(harness.effects.borrow().status_refreshes.len(), 2);
}

#[test]
fn waiting_sessions_move_first_only_on_reordering_scan() {
    let computer = worker("box", "box");
    let mut newest = session("$1", "newest");
    newest.activity = NOW;
    let older = with_agent(session("$2", "older"), AgentState::Waiting, None);
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![newest.clone(), older.clone()])],
    );
    assert_eq!(harness.picker.entries[0].sessions()[0].id, "$2");
    let newest = with_agent(newest, AgentState::Waiting, None);
    let mut older = older;
    older.agent = None;
    harness.picker.entries[0]
        .update_inventory(ready(&computer, vec![newest.clone(), older.clone()]), false);
    harness.picker.rebuild();
    assert_eq!(harness.picker.entries[0].sessions()[0].id, "$2");
    harness.picker.entries[0].update_inventory(ready(&computer, vec![older, newest]), true);
    harness.picker.rebuild();
    assert_eq!(harness.picker.entries[0].sessions()[0].id, "$1");
}

#[test]
fn errors_do_not_get_waiting_priority_on_reordering_scans() {
    let computer = worker("box", "box");
    let mut working = with_agent(session("$1", "working"), AgentState::Working, None);
    working.activity = NOW;
    let error = with_agent(session("$2", "error"), AgentState::Error, None);
    let mut waiting = with_agent(session("$3", "waiting"), AgentState::Waiting, None);
    waiting.activity = 1;
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(
            &computer,
            vec![error.clone(), working.clone(), waiting.clone()],
        )],
    );
    for _ in 0..2 {
        assert_eq!(
            harness.picker.entries[0]
                .sessions()
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["$3", "$1", "$2"],
        );
        harness.picker.entries[0].update_inventory(
            ready(
                &computer,
                vec![error.clone(), waiting.clone(), working.clone()],
            ),
            true,
        );
    }
}
