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
        let additions = effects.clone();
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
            reporting_setup: Arc::new(|_, _, _| {
                Err("Reporting setup unavailable in fixture".into())
            }),
            resolve_user: Arc::new(|_, _| Err("SSH settings unavailable".into())),
            sharing_active: Box::new(|_| true),
            save_mapping: Box::new(move |name, destination| {
                mappings
                    .borrow_mut()
                    .mappings
                    .push((name.into(), destination.into()));
                Ok(())
            }),
            add_mapping: Box::new(move |name, destination| {
                additions
                    .borrow_mut()
                    .mappings
                    .push((name.into(), destination.into()));
                Ok(())
            }),
            add_computer_unavailable: None,
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
            Command::ReportingSetup { .. } => {
                panic!("Reporting setup tests must deliver their completion explicitly")
            }
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
            Command::CompleteDirectory { .. } => {
                panic!("Directory completion tests must deliver their response explicitly")
            }
            Command::Create { .. } => {
                panic!("Creation tests must deliver their completion explicitly")
            }
            Command::End { .. } => {
                panic!("End-session tests must deliver their completion explicitly")
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
fn end_confirmation_defaults_to_cancel_and_keeps_the_captured_target() {
    let computer = remote("box", "Build computer", "alice@build.example");
    let captured = session("$0", "important work");
    let other = session("$1", "other");
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![captured.clone(), other.clone()])],
    );
    harness.press(&["x"]);
    let view = harness.rendered(100, 20);
    for text in [
        "important work ($0)",
        "Build computer",
        "alice@build.example",
    ] {
        assert!(view.contains(text), "{view}");
    }
    harness.press(&["enter"]);
    assert!(harness.picker.ending.is_none());
    assert!(harness.picker.ending_pending.is_none());
    harness.press(&["x"]);
    harness
        .picker
        .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(!harness.picker.quit);
    assert!(harness.picker.ending.is_none());

    harness.press(&["x"]);
    // A refresh may remove the selected lifetime and replace the visible row.
    let replacement = session("$2", &captured.name);
    harness.picker.update(Message::Checked(ready(
        &computer,
        vec![replacement, other.clone()],
    )));
    harness.picker.show_session(&computer.key, &other.id);
    harness.press(&["down"]);
    harness.buffer(100, 20);
    let Some(Command::End {
        worker, session, ..
    }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("expected an explicit end request");
    };
    assert_eq!(worker, computer);
    assert_eq!(session, captured);
    assert!(harness.picker.ending.as_ref().unwrap().busy);
    // Repeated Enter never sends a second destructive request.
    assert!(
        harness
            .picker
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    assert!(harness.launched().is_empty());
    assert!(harness.effects.borrow().states.is_empty());
    assert!(harness.effects.borrow().mappings.is_empty());
}

#[test]
fn end_confirmation_refuses_a_changed_endpoint_or_unfinished_scan() {
    let computer = remote("box", "box", "alice@box");
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![session("$0", "work")])],
    );
    harness.press(&["x", "tab"]);
    harness.picker.entries[0].worker.destination = Some("bob@box".into());
    harness.buffer(100, 20);
    harness.press(&["enter"]);
    assert!(harness.picker.ending_pending.is_none());
    harness.press(&["esc"]);
    harness.picker.entries[0].worker = computer;
    harness.press(&["x", "tab"]);
    let _scan = harness.picker.rescan(false);
    harness.buffer(100, 20);
    harness.press(&["enter"]);
    assert!(harness.picker.ending_pending.is_none());
}

#[test]
fn end_confirmation_cannot_execute_an_unseen_or_clipped_warning() {
    let computer = worker("box", "box");
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![session("$0", "work")])],
    );
    harness.press(&["x", "tab", "enter"]);
    assert!(harness.picker.ending_pending.is_none());
    harness.buffer(40, 6);
    harness.press(&["enter"]);
    assert!(harness.picker.ending_pending.is_none());
    harness.press(&["esc"]);
    assert!(harness.picker.ending.is_none());
}

#[test]
fn terminal_x_is_input_and_end_cancellation_never_disconnects_another_session() {
    let mut harness = terminal_harness(&[
        "/bin/sh",
        "-c",
        "read line; printf 'RECEIVED_%s\\n' \"$line\"; exec /bin/cat",
    ]);
    let started = harness.picker.active.as_ref().unwrap().started;
    harness.press(&["x", "enter"]);
    assert!(harness.picker.ending.is_none());
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
            .contains("RECEIVED_x")
        {
            break;
        }
        assert!(Instant::now() < deadline, "x was not sent to the terminal");
        std::thread::sleep(Duration::from_millis(5));
    }
    harness
        .picker
        .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness.press(&["down", "x", "tab"]);
    harness.buffer(100, 20);
    let Some(Command::End {
        worker,
        session,
        cancel,
    }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("expected an explicit end request");
    };
    assert_eq!(session.id, "$1");
    harness.press(&["esc", "x"]);
    assert!(cancel.is_cancelled());
    assert!(harness.picker.ending.is_none());
    assert!(harness.picker.ending_pending.is_some());
    assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
    // A positive receipt still proves completion even after local cancellation.
    let command = harness.picker.update(Message::Ended {
        worker,
        session,
        result: Ok(()),
    });
    assert!(matches!(command, Some(Command::Discover { .. })));
    assert!(harness.picker.ending_pending.is_none());
    assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
    assert_eq!(harness.picker.entries[0].sessions().len(), 1);
    assert!(harness.launched().is_empty());
}

#[test]
fn uncertain_end_rescans_without_retrying_or_closing_the_active_client() {
    for cancel_request in [false, true] {
        let mut harness = terminal_harness(&["/bin/cat"]);
        let started = harness.picker.active.as_ref().unwrap().started;
        harness
            .picker
            .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        harness.press(&["x", "tab"]);
        harness.buffer(100, 20);
        let Some(Command::End {
            worker, session, ..
        }) = harness
            .picker
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("expected an explicit end request");
        };
        if cancel_request {
            harness.press(&["esc"]);
        }
        let generation = harness.picker.generation;
        let command = harness.picker.update(Message::Ended {
            worker: worker.clone(),
            session: session.clone(),
            result: Err((
                Status::ProbeFailed,
                "Request interrupted; the session may already have ended.".into(),
            )),
        });
        assert!(matches!(command, Some(Command::Discover { .. })));
        assert!(harness.picker.generation > generation);
        assert!(!harness.picker.reorder_on_scan);
        assert!(harness.picker.ending.is_none());
        assert!(harness.picker.ending_pending.is_none());
        assert!(!harness.picker.notice.is_empty());
        assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
        // The old scan cannot overwrite either the inventory or scan state.
        harness.picker.update(Message::Inventory {
            generation,
            inventory: Some(ready(&worker, vec![])),
        });
        harness.picker.update(Message::Inventory {
            generation,
            inventory: None,
        });
        assert!(harness.picker.scanning);
        assert!(
            harness.picker.entries[0]
                .sessions()
                .iter()
                .any(|s| same_session(s, &session))
        );
        assert!(harness.launched().is_empty());
    }
}

#[test]
fn successful_end_closes_only_its_client_and_invalidates_older_scans() {
    for changed_connection in [false, true] {
        let mut harness = terminal_harness(&["/bin/cat"]);
        harness
            .picker
            .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        harness.press(&["x", "tab"]);
        harness.buffer(100, 20);
        let Some(Command::End {
            worker, session, ..
        }) = harness
            .picker
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("expected an explicit end request");
        };
        if changed_connection {
            // An attachment on a changed endpoint is not the captured client.
            harness.picker.active.as_mut().unwrap().worker.destination = Some("other@box".into());
        }
        let Command::Discover { generation, .. } = harness.picker.rescan(false) else {
            panic!("expected discovery");
        };
        harness.picker.update(Message::Discovered {
            generation,
            result: Ok((vec![worker.clone()], String::new())),
        });
        let command = harness.picker.update(Message::Ended {
            worker: worker.clone(),
            session: session.clone(),
            result: Ok(()),
        });
        assert!(matches!(command, Some(Command::Discover { .. })));
        assert_eq!(harness.picker.active.is_some(), changed_connection);
        assert!(!harness.picker.terminal_focused);
        harness.picker.update(Message::Inventory {
            generation,
            inventory: Some(ready(&worker, vec![session])),
        });
        harness.picker.update(Message::Inventory {
            generation,
            inventory: None,
        });
        assert!(harness.picker.scanning);
        assert_eq!(harness.picker.entries[0].sessions()[0].id, "$1");
        assert_eq!(harness.picker.entries[0].sessions().len(), 1);
        assert!(harness.launched().is_empty());
    }
}

#[test]
fn creation_cancellation_preserves_the_client_and_never_attaches_late_success() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let started = harness.picker.active.as_ref().unwrap().started;
    harness
        .picker
        .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness.press(&["n", "new-work"]);
    let Some(Command::Create { worker, cancel, .. }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("expected explicit creation");
    };
    harness.press(&["enter"]);
    assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
    harness.press(&["esc", "n"]);
    assert!(cancel.is_cancelled());
    assert!(harness.picker.creation.is_none());
    assert!(harness.picker.creation_pending.is_some());
    let created = session("$2", "new-work");
    let result = harness.picker.update(Message::Created {
        worker,
        result: Ok((created.clone(), vec![created])),
    });
    assert!(matches!(result, Some(Command::Discover { .. })));
    assert!(!harness.picker.reorder_on_scan);
    assert!(harness.picker.creation_pending.is_none());
    assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
    assert!(!harness.picker.terminal_focused);
    assert!(harness.launched().is_empty());
}

#[test]
fn creation_requires_a_ready_unchanged_computer() {
    let computer = remote("box", "box", "alice@box");
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![failed(&computer, Status::AuthRequired, "denied")],
    );
    harness.press(&["n"]);
    assert!(harness.picker.creation.is_none());
    assert!(harness.picker.creation_pending.is_none());
    harness
        .picker
        .update(Message::Checked(ready(&computer, vec![])));
    harness.press(&["n", "new-work"]);
    harness.picker.entries[0].worker.destination = Some("bob@box".into());
    harness.press(&["enter"]);
    assert!(harness.picker.creation_pending.is_none());
    assert!(!harness.picker.creation.as_ref().unwrap().busy);
    harness.press(&["esc", "n", "new-work"]);
    let Some(Command::Create { worker, .. }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("expected creation on changed computer");
    };
    harness.picker.entries[0].worker.destination = Some("carol@box".into());
    let created = session("$2", "new-work");
    let result = harness.picker.update(Message::Created {
        worker,
        result: Ok((created.clone(), vec![created])),
    });
    assert!(matches!(result, Some(Command::Discover { .. })));
    assert!(harness.picker.entries[0].sessions().is_empty());
    assert!(harness.launched().is_empty());
}

#[test]
fn creation_success_appends_without_reordering_or_pinning_and_selects_exact_identity() {
    let computer = worker("box", "box");
    let mut first = session("$0", "first");
    first.activity = NOW;
    let mut second = session("$1", "second");
    second.activity = NOW - 1;
    // The form starts in the selected session's directory.
    second.path = "/srv/second".into();
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![first.clone(), second.clone()])],
    );
    harness.press(&["/", "second", "enter", "n", "new-work"]);
    let Some(Command::Create {
        worker, request, ..
    }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("expected explicit creation");
    };
    assert_eq!(request.directory, second.path);
    let mut created = session("$2", "new-work");
    created.created += 1;
    second.activity = NOW + 1;
    let result = harness.picker.update(Message::Created {
        worker,
        result: Ok((created.clone(), vec![created.clone(), second, first])),
    });
    harness.drive(result);
    let launched = harness.launched();
    assert_eq!(launched.len(), 1);
    assert_eq!(launched[0].session.as_ref(), Some(&created));
    let ids: Vec<_> = harness.picker.entries[0]
        .sessions()
        .iter()
        .map(|session| session.id.as_str())
        .collect();
    assert_eq!(ids, ["$0", "$1", "$2"]);
    assert_eq!(harness.picker.selected_session().unwrap().1, created);
    assert!(harness.picker.filter.is_empty());
    assert!(harness.picker.creation.is_none());
    assert!(harness.picker.creation_pending.is_none());
    assert!(harness.picker.state.pins.is_empty());
    assert!(harness.effects.borrow().states.is_empty());
}

#[test]
fn creation_success_rejects_an_older_inflight_scan() {
    let computer = worker("box", "box");
    let other = worker("other", "other");
    let mut harness = Harness::new(
        vec![computer.clone(), other.clone()],
        vec![ready(&computer, vec![]), ready(&other, vec![])],
    );
    // Initial probe completions can reorder equally ready computers.
    harness.picker.selection = Selection::Computer(computer.key.clone());
    harness.picker.rebuild();
    harness.press(&["n", "new-work"]);
    let Some(Command::Create { worker, .. }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("expected explicit creation");
    };
    assert_eq!(worker.key, computer.key);
    // An existing client's exit can refresh while creation is in flight.
    let Command::Discover { generation, .. } = harness.picker.rescan(false) else {
        panic!("expected discovery");
    };
    let result = harness.picker.update(Message::Discovered {
        generation,
        result: Ok((vec![computer.clone(), other.clone()], String::new())),
    });
    assert!(matches!(result, Some(Command::Scan { .. })));
    let created = session("$2", "new-work");
    let result = harness.picker.update(Message::Created {
        worker,
        result: Ok((created.clone(), vec![created.clone()])),
    });
    harness.drive(result);
    harness.picker.update(Message::Inventory {
        generation,
        inventory: Some(ready(&computer, vec![])),
    });
    let other_session = session("$3", "other-work");
    harness.picker.update(Message::Inventory {
        generation,
        inventory: Some(ready(&other, vec![other_session.clone()])),
    });
    harness.picker.update(Message::Inventory {
        generation,
        inventory: None,
    });
    assert!(!harness.picker.scanning);
    assert!(harness.picker.pending.is_empty());
    assert_eq!(harness.picker.selected_session().unwrap().1, created);
    let other_entry = harness
        .picker
        .entries
        .iter()
        .find(|entry| entry.worker.key == other.key)
        .unwrap();
    assert_eq!(other_entry.sessions(), &[other_session]);
    assert_eq!(harness.launched().len(), 1);
}

#[test]
fn creation_failure_rescans_without_retrying_or_replacing_the_client() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let started = harness.picker.active.as_ref().unwrap().started;
    harness
        .picker
        .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness.press(&["n", "new-work"]);
    let Some(Command::Create { worker, .. }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("expected explicit creation");
    };
    let result = harness.picker.update(Message::Created {
        worker,
        result: Err((Status::ProbeFailed, "Command timed out after 12s".into())),
    });
    assert!(matches!(result, Some(Command::Discover { .. })));
    assert!(!harness.picker.reorder_on_scan);
    assert!(
        harness
            .picker
            .creation
            .as_ref()
            .unwrap()
            .error
            .contains("timed out")
    );
    // A repeated Enter must not race the required uncertainty rescan.
    harness.press(&["enter"]);
    assert!(harness.picker.creation_pending.is_none());
    assert!(!harness.picker.creation.as_ref().unwrap().busy);
    harness.drive(result);
    assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
    assert!(harness.picker.creation_pending.is_none());
    let form = harness.picker.creation.as_ref().unwrap();
    assert!(!form.busy);
    assert!(form.error.contains("rescan"));
    assert!(matches!(
        harness
            .picker
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(Command::Create { .. })
    ));
    assert!(harness.launched().is_empty());
}

#[test]
fn creation_form_owns_keys_paste_and_wheel_in_all_layouts() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let started = harness.picker.active.as_ref().unwrap().started;
    harness
        .picker
        .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness.press(&["n"]);
    harness.picker.update(Message::Paste("new-work\n\t".into()));
    let selection = harness.picker.selection.clone();
    for (width, height) in [(120, 30), (70, 40), (40, 12), (3, 3), (1, 1)] {
        harness.buffer(width, height);
        assert!(!harness.picker.terminal_visible);
        assert!(!harness.picker.preview_visible);
        assert!(harness.picker.inactive_scroll_hint().is_none());
        let area = harness.picker.terminal_area;
        harness.picker.update(Message::Mouse(wheel(
            MouseEventKind::ScrollDown,
            area.x,
            area.y,
        )));
        let area = harness.picker.list_area;
        harness.picker.update(Message::Mouse(wheel(
            MouseEventKind::ScrollDown,
            area.x,
            area.y,
        )));
        assert_eq!(harness.picker.selection, selection);
        assert!(!harness.picker.show_scroll_hint);
    }
    let Some(Command::Create { request, .. }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("expected explicit creation");
    };
    assert_eq!(request.name, "new-work");
    harness
        .picker
        .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(!harness.picker.quit);
    assert!(harness.picker.creation.is_none());
    assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
}

#[test]
fn terminal_focus_keeps_shortcuts_and_paste_out_of_the_picker() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let generation = harness.picker.generation;
    harness.press(&["q", "r", "n", "tab", "/", "1", "2"]);
    assert!(!harness.picker.quit);
    assert!(!harness.picker.filtering);
    assert!(harness.picker.creation.is_none());
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
fn sidebar_groups_unavailable_computers_and_skips_the_divider() {
    let up = worker("up", "working");
    let empty = remote("empty", "empty", "empty.tail");
    let probing = remote("probing", "probing", "probing.tail");
    let statuses = [
        ("auth", Status::AuthRequired),
        ("trust", Status::HostTrust),
        ("tmux", Status::MissingTmux),
        ("tools", Status::MissingTools),
        ("probe", Status::ProbeFailed),
        ("unreachable", Status::Unreachable),
    ];
    let mut workers = vec![up.clone(), empty.clone(), probing];
    let mut inventories = vec![
        ready(&up, vec![session("$0", "work")]),
        ready(&empty, vec![]),
    ];
    for (name, status) in statuses {
        let worker = remote(name, name, &format!("{name}.tail"));
        inventories.push(failed(&worker, status, "Access unavailable"));
        workers.push(worker);
    }
    let mut harness = Harness::new(vec![], vec![]);
    let _ = harness.picker.rescan(true);
    harness.picker.update(Message::Discovered {
        generation: harness.picker.generation,
        result: Ok((workers, String::new())),
    });
    for inventory in inventories {
        harness.picker.update(Message::Inventory {
            generation: harness.picker.generation,
            inventory: Some(inventory),
        });
    }
    let view = harness.rendered(100, 30);
    let divider = view.find("Other computers").expect("unavailable section");
    for name in ["working", "empty", "probing"] {
        assert!(view.find(name).unwrap() < divider, "{name}:\n{view}");
    }
    for (name, _) in statuses {
        assert!(view.find(name).unwrap() > divider, "{name}:\n{view}");
    }
    let lines: Vec<_> = view.lines().collect();
    let heading = lines
        .iter()
        .position(|line| line.contains("Other computers"))
        .unwrap();
    assert!(
        lines[heading - 1]
            .chars()
            .skip(1)
            .take(30)
            .all(|ch| ch == ' '),
        "a blank sidebar line separates the sections:\n{view}"
    );
    harness.press(&["/", "empty", "enter", "esc", "down"]);
    assert_eq!(harness.picker.selection, Selection::Computer("auth".into()));
    harness.press(&["up"]);
    assert_eq!(
        harness.picker.selection,
        Selection::Computer("empty".into())
    );
    harness.press(&["/", "working"]);
    assert!(!harness.rendered(100, 20).contains("Other computers"));
    harness.press(&["esc", "/", "auth"]);
    let view = harness.rendered(100, 20);
    assert!(
        view.lines().nth(2).unwrap().contains("Other computers"),
        "{view}"
    );
    harness.press(&["enter", "enter"]);
    assert_eq!(harness.picker.input.as_ref().unwrap().key, "auth");
}

#[test]
fn sidebar_status_moves_keep_the_selected_computer() {
    let working = worker("working", "working");
    let recovering = remote("recovering", "recovering", "recovering.tail");
    let mut harness = Harness::new(
        vec![working.clone(), recovering.clone()],
        vec![
            ready(&working, vec![session("$0", "work")]),
            failed(&recovering, Status::AuthRequired, "Sign in"),
        ],
    );
    harness.press(&["down"]);
    assert_eq!(
        harness.picker.selection,
        Selection::Computer("recovering".into())
    );
    harness.picker.update(Message::Checked(ready(
        &recovering,
        vec![session("$1", "first"), session("$2", "recovered")],
    )));
    assert_eq!(
        harness.picker.selected_session().unwrap().0.key,
        "recovering"
    );
    assert!(!harness.rendered(100, 20).contains("Other computers"));
    harness.press(&["down"]);
    let selected = harness.picker.selection.clone();
    harness.picker.update(Message::Checked(failed(
        &recovering,
        Status::Unreachable,
        "Connection lost",
    )));
    let row = harness.picker.current_row().unwrap();
    assert!(row.kind == RowKind::Computer);
    assert_eq!(harness.picker.entries[row.entry].worker.key, "recovering");
    assert_eq!(harness.picker.selection, selected);
    harness.picker.update(Message::Checked(ready(
        &recovering,
        vec![session("$1", "first"), session("$2", "recovered")],
    )));
    assert_eq!(harness.picker.selection, selected);
    assert_eq!(
        harness.picker.selected_session().unwrap().1.name,
        "recovered"
    );
    harness.press(&["up", "up", "enter"]);
    assert_eq!(harness.launched()[0].worker.key, "working");
}

#[test]
fn known_offline_computers_start_below_the_divider_but_can_recover() {
    let working = worker("working", "working");
    let mut offline = remote("offline", "offline-box", "offline.tail");
    offline.online = Some(false);
    // Discovery knows the peer is offline before any SSH result arrives.
    let mut harness = Harness::new(
        vec![offline.clone(), working.clone()],
        vec![ready(&working, vec![session("$0", "work")])],
    );
    let view = harness.rendered(100, 20);
    let divider = view.find("Other computers").expect("offline section");
    assert!(view.find("working").unwrap() < divider, "{view}");
    assert!(view.find("offline-box").unwrap() > divider, "{view}");
    harness.press(&["down"]);
    assert_eq!(
        harness.picker.selection,
        Selection::Computer("offline".into())
    );
    // A real successful probe wins over Tailscale's stale offline hint.
    harness.picker.update(Message::Checked(ready(
        &offline,
        vec![session("$1", "recovered")],
    )));
    assert_eq!(harness.picker.selected_session().unwrap().0.key, "offline");
    assert!(!harness.rendered(100, 20).contains("Other computers"));
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
    let order: Vec<usize> = ["has-work", "no-work", "needs-key", "gone"]
        .iter()
        .map(|name| view.find(name).unwrap_or_else(|| panic!("{name}:\n{view}")))
        .collect();
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{view}");
    harness.press(&["down", "down"]);
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
    // Enter on an empty computer opens a form without creating anything.
    harness.press(&["up", "enter"]);
    assert_eq!(harness.launched().len(), 1);
    assert_eq!(
        harness.picker.creation.as_ref().unwrap().worker.key,
        "empty"
    );
    assert!(harness.picker.creation_pending.is_none());
    // Completion expands the computer without stealing selection while browsing.
    harness.picker.update(Message::Checked(ready(
        &launched[0].worker,
        vec![session("$9", "new-work")],
    )));
    assert_eq!(
        harness.picker.selection,
        Selection::Computer("empty".into())
    );
    harness.press(&["esc", "up", "enter"]);
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
        &["alpha", "first", "second", "removed", "beta", "gamma"],
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
        &["first", "second", "removed", "beta", "gamma"],
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
        &["alpha", "beta", "beta-newer", "beta-older", "gamma"],
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
            "alpha", "newest", "second", "added", "first", "beta", "delta",
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
    // At every width, the widget receives complete hints, not clipped spans.
    for width in 1..=80 {
        let line = harness.picker.footer_line(width);
        assert!(line.width() <= usize::from(width));
        assert!(line.spans.len().is_multiple_of(3));
    }
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
fn adding_saves_mapping_rescans_without_reordering_and_selects_new_computer() {
    for (status, has_sessions) in [
        (Status::Ready, true),
        (Status::Ready, false),
        (Status::AuthRequired, false),
        (Status::HostTrust, false),
        (Status::Unreachable, false),
    ] {
        let root = crate::test_support::TempDir::new();
        let path = root.path().join("custom-cli.json");
        let path = path.to_str().unwrap().to_owned();
        let alpha = remote("alpha", "alpha", "alpha");
        let beta = remote("beta", "beta", "beta");
        let added = remote("alice@build-box", "alice@build-box", "alice@build-box");
        let mut harness = Harness::new(
            vec![alpha.clone(), beta.clone()],
            vec![ready(&alpha, vec![]), ready(&beta, vec![])],
        );
        let saved_path = path.clone();
        let effects = harness.effects.clone();
        harness.picker.services.add_mapping = Box::new(move |name, destination| {
            crate::workers::add_host_mapping(&saved_path, name, destination)?;
            effects
                .borrow_mut()
                .mappings
                .push((name.into(), destination.into()));
            Ok(())
        });
        let discovery_path = path.clone();
        let existing = vec![beta.clone(), alpha.clone()];
        harness.picker.services.discover = Arc::new(move |_| {
            let mut workers = existing.clone();
            workers.extend(crate::workers::configured_workers(
                &load_config(&discovery_path)?,
                &HashSet::new(),
            ));
            Ok((workers, String::new()))
        });
        let mut inventory = failed(&added, status, "probe result");
        if has_sessions {
            inventory.sessions.push(session("$0", "project"));
        }
        harness.picker.services.scan = Arc::new(move |_, workers| {
            assert!(workers.iter().any(|worker| worker.key == "alice@build-box"));
            let (sender, receiver) = mpsc::channel();
            sender
                .send(ready(&beta, vec![session("$1", "work")]))
                .unwrap();
            sender.send(inventory.clone()).unwrap();
            sender
                .send(failed(&alpha, Status::Unreachable, "offline"))
                .unwrap();
            receiver
        });
        // Existing computers must keep whatever order they had before the add.
        let mut expected: Vec<String> = harness
            .picker
            .entries
            .iter()
            .map(|entry| entry.worker.key.clone())
            .collect();
        expected.push("alice@build-box".into());
        harness.press(&["/", "alpha", "enter", "a", "alice@build-box"]);
        let view = harness.rendered(120, 24);
        assert!(view.contains("Add SSH destination/alias:"), "{view}");
        assert!(
            view.contains("enter save") && view.contains("esc cancel"),
            "{view}"
        );
        let generation = harness.picker.generation;
        let command = harness
            .picker
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(command, Some(Command::Discover { .. })));
        assert!(!harness.picker.reorder_on_scan);
        assert_eq!(harness.picker.generation, generation + 1);
        assert!(harness.picker.add_input.is_none());
        harness.drive(command);
        assert_eq!(
            harness.effects.borrow().mappings,
            vec![("alice@build-box".into(), "alice@build-box".into())],
        );
        assert_eq!(
            load_config(&path).unwrap().hosts["alice@build-box"],
            "alice@build-box",
        );
        assert_eq!(
            harness
                .picker
                .entries
                .iter()
                .map(|entry| entry.worker.key.clone())
                .collect::<Vec<_>>(),
            expected,
        );
        let row = harness.picker.current_row().unwrap();
        assert_eq!(harness.picker.entries[row.entry].worker, added);
        assert_eq!(harness.picker.entries[row.entry].status(), status);
        assert_eq!(harness.picker.session(row).is_some(), has_sessions);
        assert!(harness.picker.filter.is_empty());
        assert!(harness.launched().is_empty());
    }
}

#[test]
fn adding_invalid_destination_keeps_input_open_without_saving() {
    for destination in ["", "-host", "bad host", "box;id", "é", &"a".repeat(1025)] {
        let mut harness = Harness::new(vec![], vec![]);
        let generation = harness.picker.generation;
        harness.press(&["a", destination, "enter"]);
        assert_eq!(harness.picker.add_input.as_deref(), Some(destination));
        assert!(harness.picker.notice.contains("SSH destination"));
        assert!(harness.effects.borrow().mappings.is_empty());
        assert_eq!(harness.picker.generation, generation);
    }
}

#[test]
fn adding_duplicate_computer_name_key_or_destination_is_rejected() {
    for destination in ["build", "node-1", "alice@build-box", "local"] {
        let computer = remote("node-1", "build", "alice@build-box");
        let mut harness = Harness::new(vec![computer.clone()], vec![ready(&computer, vec![])]);
        let generation = harness.picker.generation;
        harness.press(&["a", destination, "enter"]);
        assert_eq!(harness.picker.add_input.as_deref(), Some(destination));
        assert!(harness.picker.notice.contains("already exists"));
        assert!(harness.effects.borrow().mappings.is_empty());
        assert_eq!(harness.picker.generation, generation);
    }
}

#[test]
fn cancelling_add_computer_keeps_list_and_configuration_unchanged() {
    let computer = remote("box", "box", "box");
    let mut harness = Harness::new(vec![computer.clone()], vec![ready(&computer, vec![])]);
    let selection = harness.picker.selection.clone();
    let generation = harness.picker.generation;
    harness.press(&["a", "new-box", "esc"]);
    assert!(harness.picker.add_input.is_none());
    assert!(harness.picker.notice.is_empty());
    assert!(harness.effects.borrow().mappings.is_empty());
    assert_eq!(harness.picker.selection, selection);
    assert_eq!(harness.picker.generation, generation);
    assert_eq!(harness.picker.entries.len(), 1);
    assert_eq!(harness.picker.entries[0].worker, computer);
}

#[test]
fn add_shortcut_is_only_active_in_the_list() {
    let computer = remote("box", "box", "box");
    let mut harness = Harness::new(vec![computer.clone()], vec![ready(&computer, vec![])]);
    harness.picker.terminal_focused = true;
    harness.press(&["a"]);
    assert!(harness.picker.add_input.is_none());
    harness.picker.terminal_focused = false;
    harness.press(&["/", "a"]);
    assert_eq!(harness.picker.filter, "a");
    assert!(harness.picker.add_input.is_none());
    harness.press(&["esc", "u", "a"]);
    assert_eq!(harness.picker.input.as_ref().unwrap().text, "a");
    assert!(harness.picker.add_input.is_none());
    harness.press(&["esc", "a", "a"]);
    assert_eq!(harness.picker.add_input.as_deref(), Some("a"));
    assert!(harness.picker.input.is_none());
    assert!(harness.effects.borrow().mappings.is_empty());
}

#[test]
fn adding_filters_pasted_controls_and_supports_editing() {
    let mut harness = Harness::new(vec![], vec![]);
    harness.press(&["a"]);
    harness
        .picker
        .update(Message::Paste("alice@\nbuild\t-box\r".into()));
    assert_eq!(harness.picker.add_input.as_deref(), Some("alice@build-box"));
    harness.press(&["backspace"]);
    assert_eq!(harness.picker.add_input.as_deref(), Some("alice@build-bo"));
    harness
        .picker
        .key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(harness.picker.add_input.as_deref(), Some(""));
    harness.press(&["ssh-alias", "enter"]);
    assert_eq!(
        harness.effects.borrow().mappings,
        vec![("ssh-alias".into(), "ssh-alias".into())],
    );
}

#[test]
fn add_computer_save_failure_keeps_input_and_list_unchanged() {
    let computer = remote("box", "box", "box");
    let mut harness = Harness::new(vec![computer.clone()], vec![ready(&computer, vec![])]);
    let selection = harness.picker.selection.clone();
    let generation = harness.picker.generation;
    harness.picker.services.add_mapping = Box::new(|_, _| Err("read-only config".into()));
    harness.press(&["a", "new-box", "enter"]);
    assert!(
        harness
            .picker
            .notice
            .contains("Could not save cli.json: read-only config")
    );
    assert_eq!(harness.picker.add_input.as_deref(), Some("new-box"));
    assert!(harness.effects.borrow().mappings.is_empty());
    assert_eq!(harness.picker.generation, generation);
    assert_eq!(harness.picker.selection, selection);
    assert_eq!(harness.picker.entries.len(), 1);
    assert_eq!(harness.picker.entries[0].worker, computer);
    assert!(harness.picker.select_added.is_none());
}

#[test]
fn adding_is_unavailable_when_flags_exclude_configured_hosts() {
    use crate::options::Options;

    for (opts, flag) in [
        (
            Options {
                local_only: true,
                ..Options::default()
            },
            "--local",
        ),
        (
            Options {
                hosts: vec!["box".into()],
                ..Options::default()
            },
            "--host",
        ),
        (
            Options {
                hosts: vec!["box".into()],
                no_local: true,
                ..Options::default()
            },
            "--host",
        ),
    ] {
        let mut harness = Harness::new(vec![], vec![]);
        harness.picker.services.add_computer_unavailable = add_computer_unavailable(&opts);
        let generation = harness.picker.generation;
        harness.press(&["a"]);
        assert!(harness.picker.add_input.is_none());
        assert!(harness.picker.notice.contains(flag));
        assert!(harness.picker.notice.contains("Restart without"));
        assert!(harness.effects.borrow().mappings.is_empty());
        assert_eq!(harness.picker.generation, generation);
    }
    for no_local in [false, true] {
        let mut harness = Harness::new(vec![], vec![]);
        harness.picker.services.add_computer_unavailable = add_computer_unavailable(&Options {
            no_local,
            ..Options::default()
        });
        harness.press(&["a", "new-box", "enter"]);
        assert!(harness.picker.add_input.is_none());
        assert_eq!(
            harness.effects.borrow().mappings,
            vec![("new-box".into(), "new-box".into())],
        );
    }
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
    let computers = sidebar
        .iter()
        .position(|line| line.trim() == "Computers")
        .expect("computer section");
    assert_eq!(computers, header + 5, "one blank line after the pins");
    assert!(sidebar[computers - 1].trim().is_empty());
    assert!(sidebar[computers + 1].contains("studio"));
    harness.press(&["down", "down", "down", "enter"]);
    let launched = harness.launched();
    assert_eq!(launched[0].worker.key, "a");
    assert_eq!(launched[0].session.as_ref().unwrap().id, "$0");
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
                "?",
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
            let marker_cell = &buffer[((x - 4) as u16, y as u16)];
            assert_eq!(marker_cell.symbol(), marker);
            assert_eq!(marker_cell.fg, style.fg.unwrap());
            if name == "waiting" {
                assert!(marker_cell.modifier.contains(Modifier::BOLD));
            }
        }
        assert!(name_columns.windows(2).all(|pair| pair[0] == pair[1]));
        assert!(age_columns.windows(2).all(|pair| pair[0] == pair[1]));
    }
    harness.picker.cursor = harness.picker.rows.iter().position(|row| {
        row.kind == RowKind::Session
            && harness
                .picker
                .session(*row)
                .is_some_and(|session| session.name == "waiting")
    });
    harness.picker.remember();
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
                AgentState::Working => "●",
                AgentState::Waiting => "?",
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
fn agent_refresh_keeps_unselected_native_working_visible_with_recent_output() {
    let computer = worker("box", "box");
    let mut background = with_agent(session("$2", "background"), AgentState::Working, None);
    background.output = NOW - 601;
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(
            &computer,
            vec![session("$1", "selected"), background],
        )],
    );
    assert!(!harness.rendered(120, 30).contains("●   background"));
    let selection = harness.picker.selection.clone();
    for output in [NOW, NOW - 700] {
        harness.picker.update(Message::AgentStatuses {
            generation: harness.picker.generation,
            results: vec![(
                computer.clone(),
                Ok(vec![AgentStatusRow {
                    server_pid: 1,
                    server_start: 1,
                    session_id: "$2".into(),
                    output,
                    agent: crate::tmux::parse_agent_status("working"),
                }]),
            )],
        });
        assert!(harness.rendered(120, 30).contains("●   background"));
        assert_eq!(harness.picker.selection, selection);
        assert_eq!(harness.picker.entries[0].sessions()[1].output, NOW);
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
            output: NOW,
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
    assert!(view.contains("?   second"));
    assert!(view.contains("1 waiting"));
    harness.picker.update(Message::AgentStatuses {
        generation: harness.picker.generation,
        results: vec![(
            computer,
            Ok(vec![AgentStatusRow {
                server_pid: 1,
                server_start: 1,
                session_id: "$2".into(),
                output: NOW,
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
    assert!(view.contains("✗   second"));
    assert!(!view.contains("1 waiting"));
}

#[test]
fn agent_refresh_rejects_stale_identities_and_invalidates_failed_checks() {
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
                    output: NOW,
                    agent: Some(crate::tmux::AgentStatus {
                        state: AgentState::Waiting,
                        since: None,
                    }),
                }]),
            )],
        });
        let mut expected = original.clone();
        if generation == harness.picker.generation {
            expected.agent = None;
        }
        assert_eq!(harness.picker.entries[0].sessions(), &[expected]);
    }
    harness.picker.update(Message::AgentStatuses {
        generation,
        results: vec![(computer, Err("untrusted failure".into()))],
    });
    let mut unreported = original;
    unreported.agent = None;
    assert_eq!(harness.picker.entries[0].sessions(), &[unreported]);
    assert_eq!(harness.picker.coverage().unavailable, 1);
    assert!(harness.picker.notice.is_empty());
}

#[test]
fn agent_refresh_requires_ready_sessions_including_unshared_remotes() {
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
        ["local", "shared", "unshared"]
    );
    harness.picker.scanning = true;
    harness.tick(start + STATUS_REFRESH * 3);
    assert_eq!(harness.effects.borrow().status_refreshes.len(), 3);
    harness.picker.scanning = false;
    harness.picker.quit();
    harness.tick(start + STATUS_REFRESH * 4);
    assert_eq!(harness.effects.borrow().status_refreshes.len(), 3);
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

fn attention_sessions(harness: &Harness) -> Vec<(String, String)> {
    harness
        .picker
        .rows
        .iter()
        .filter_map(|row| {
            harness.picker.session(*row).map(|session| {
                (
                    harness.picker.entries[row.entry].worker.key.clone(),
                    session.id.clone(),
                )
            })
        })
        .collect()
}

fn status_rows(sessions: &[Session]) -> Vec<AgentStatusRow> {
    sessions
        .iter()
        .map(|session| AgentStatusRow {
            server_pid: session.server_pid,
            server_start: session.server_start,
            session_id: session.id.clone(),
            output: session.output,
            agent: session.agent,
        })
        .collect()
}

#[test]
fn attention_is_cross_computer_with_separate_errors_and_exact_attach_targets() {
    let alpha = worker("alpha", "alpha-host");
    let beta = remote("beta", "beta-host", "beta.example");
    let waiting = with_agent(session("$1", "needs-review"), AgentState::Waiting, None);
    let error = with_agent(session("$2", "failed-task"), AgentState::Error, None);
    let mut harness = Harness::new(
        vec![alpha.clone(), beta.clone()],
        vec![
            ready(
                &alpha,
                vec![
                    waiting.clone(),
                    with_agent(session("$3", "working-task"), AgentState::Working, None),
                ],
            ),
            ready(
                &beta,
                vec![
                    waiting.clone(),
                    error.clone(),
                    session("$4", "unreported-task"),
                ],
            ),
        ],
    );
    assert_eq!(harness.picker.view, View::Browse);
    harness.press(&["v"]);
    assert_eq!(
        attention_sessions(&harness),
        [("alpha".into(), "$1".into()), ("beta".into(), "$1".into())]
    );
    let view = harness.rendered(120, 30);
    assert!(view.contains("alpha-host") && view.contains("beta-host"));
    assert!(
        !view.contains("working-task")
            && !view.contains("unreported-task")
            && !view.contains("failed-task")
    );
    harness.press(&["e"]);
    assert_eq!(
        attention_sessions(&harness).last(),
        Some(&("beta".into(), "$2".into()))
    );
    harness.press(&["/", "beta", "enter"]);
    assert_eq!(harness.picker.attention_count(AgentState::Waiting), 2);
    assert_eq!(
        attention_sessions(&harness),
        [("beta".into(), "$1".into()), ("beta".into(), "$2".into())]
    );
    harness.press(&["enter"]);
    assert_eq!(harness.launched().len(), 1);
    assert_eq!(harness.launched()[0].worker, beta);
    assert_eq!(harness.launched()[0].session.as_ref(), Some(&waiting));
    assert_eq!(harness.picker.attention_count(AgentState::Waiting), 2);
    harness.press(&["v"]);
    assert!(harness.picker.filter.is_empty());
    harness.press(&["v"]);
    assert_eq!(harness.picker.filter, "beta");
}

#[test]
fn attention_rows_distinguish_same_named_sessions_when_computer_names_fit() {
    let workers = vec![
        worker("alpha", "attention-alpha"),
        worker("beta", "attention-beta"),
    ];
    let waiting = with_agent(session("$1", "review"), AgentState::Waiting, None);
    let inventories = workers
        .iter()
        .map(|worker| ready(worker, vec![waiting.clone()]))
        .collect();
    let mut harness = Harness::new(workers, inventories);
    harness.press(&["v"]);
    let buffer = harness.buffer(100, 20);
    let sidebar: Vec<String> = (0..20u16)
        .map(|y| (1..31u16).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    for host in ["attention-alpha", "attention-beta"] {
        assert!(
            sidebar
                .iter()
                .any(|line| line.contains("review") && line.trim_end().ends_with(host)),
            "{host}: {sidebar:#?}"
        );
    }
}

#[test]
fn attention_queue_retains_order_and_selection_without_retargeting_removed_rows() {
    let alpha = worker("alpha", "alpha");
    let beta = worker("beta", "beta");
    let first = with_agent(session("$1", "first"), AgentState::Waiting, None);
    let second = with_agent(session("$2", "second"), AgentState::Waiting, None);
    let later = session("$3", "later");
    let mut harness = Harness::new(
        vec![alpha.clone(), beta.clone()],
        vec![
            ready(&alpha, vec![first.clone(), later.clone()]),
            ready(&beta, vec![second.clone()]),
        ],
    );
    harness.press(&["v", "down"]);
    let selected = harness.picker.selection.clone();
    let later = with_agent(later, AgentState::Waiting, None);
    harness.picker.update(Message::AgentStatuses {
        generation: harness.picker.generation,
        results: vec![(
            alpha.clone(),
            Ok(status_rows(&[first.clone(), later.clone()])),
        )],
    });
    assert_eq!(
        attention_sessions(&harness),
        [
            ("alpha".into(), "$1".into()),
            ("beta".into(), "$2".into()),
            ("alpha".into(), "$3".into()),
        ]
    );
    assert_eq!(harness.picker.selection, selected);
    harness.picker.update(Message::AgentStatuses {
        generation: harness.picker.generation,
        results: vec![(
            beta.clone(),
            Ok(status_rows(&[with_agent(
                second.clone(),
                AgentState::Done,
                None,
            )])),
        )],
    });
    assert!(harness.picker.current_row().is_none());
    assert_eq!(harness.picker.selection, selected);
    harness.press(&["enter"]);
    assert!(harness.launched().is_empty());
    harness.picker.update(Message::AgentStatuses {
        generation: harness.picker.generation,
        results: vec![(beta, Ok(status_rows(&[second])))],
    });
    assert_eq!(harness.picker.selected_session().unwrap().0.key, "beta");
    assert_eq!(
        attention_sessions(&harness).last(),
        Some(&("beta".into(), "$2".into()))
    );
}

#[test]
fn attention_errors_autoexpand_after_waiting_clears_but_remain_collapsible() {
    let computer = worker("box", "box");
    let waiting = with_agent(session("$1", "waiting-task"), AgentState::Waiting, None);
    let error = with_agent(session("$2", "error-task"), AgentState::Error, None);
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![waiting.clone(), error.clone()])],
    );
    harness.press(&["v"]);
    assert_eq!(attention_sessions(&harness).len(), 1);
    harness.picker.update(Message::AgentStatuses {
        generation: harness.picker.generation,
        results: vec![(
            computer,
            Ok(status_rows(&[
                with_agent(waiting, AgentState::Done, None),
                error,
            ])),
        )],
    });
    assert_eq!(attention_sessions(&harness), [("box".into(), "$2".into())]);
    assert_eq!(harness.picker.attention_count(AgentState::Waiting), 0);
    harness.press(&["e"]);
    assert!(attention_sessions(&harness).is_empty());
    harness.press(&["e", "down", "enter"]);
    assert_eq!(harness.launched()[0].session.as_ref().unwrap().id, "$2");
}

#[test]
fn attention_coverage_distinguishes_unreported_failed_check_and_discovery() {
    let good = worker("good", "good");
    let bad = remote("bad", "bad", "bad.example");
    let waiting = with_agent(session("$1", "waiting"), AgentState::Waiting, None);
    let mut harness = Harness::new(
        vec![good.clone(), bad.clone()],
        vec![
            ready(&good, vec![waiting.clone(), session("$2", "unknown")]),
            failed(&bad, Status::AuthRequired, "sign in"),
        ],
    );
    harness.press(&["v"]);
    assert_eq!(
        harness.picker.coverage(),
        attention::Coverage {
            checked: 1,
            checking: 0,
            unavailable: 1,
            unreported: 1,
            discovery_incomplete: false,
        }
    );
    harness.picker.update(Message::AgentStatuses {
        generation: harness.picker.generation,
        results: vec![(good.clone(), Err("status request failed".into()))],
    });
    assert_eq!(harness.picker.attention_count(AgentState::Waiting), 0);
    assert_eq!(harness.picker.coverage().unavailable, 2);
    assert_eq!(harness.picker.coverage().unreported, 2);
    assert!(attention_sessions(&harness).is_empty());
    assert!(harness.picker.current_row().is_none());
    harness.press(&["c", "down"]);
    assert_eq!(harness.picker.screen().signal, Signal::Lost);
    assert!(matches!(
        harness.picker.activate(),
        Some(Command::Discover { .. })
    ));
    assert!(harness.picker.coverage().discovery_incomplete);
    harness.picker.update(Message::Discovered {
        generation: harness.picker.generation,
        result: Ok((vec![good.clone(), bad.clone()], "discovery partial".into())),
    });
    assert_eq!(harness.picker.coverage().checking, 2);
    harness.picker.update(Message::Inventory {
        generation: harness.picker.generation,
        inventory: Some(ready(&good, vec![waiting])),
    });
    assert_eq!(harness.picker.attention_count(AgentState::Waiting), 1);
    harness.picker.update(Message::Inventory {
        generation: harness.picker.generation,
        inventory: None,
    });
    assert_eq!(harness.picker.coverage().checked, 1);
    assert_eq!(harness.picker.coverage().unavailable, 1);
    assert!(harness.picker.coverage().discovery_incomplete);
    harness.picker.selection = Selection::Computer(bad.key);
    harness.picker.rebuild();
    harness.press(&["enter"]);
    assert!(harness.picker.input.is_some());
    assert!(harness.launched().is_empty());
}

#[test]
fn attention_empty_startup_and_zero_computers_remain_navigable() {
    let mut harness = Harness::new(vec![], vec![]);
    harness.press(&["v"]);
    assert!(harness.picker.current_row().is_none());
    assert_eq!(harness.picker.attention_count(AgentState::Waiting), 0);
    harness.buffer(60, 20);
    let command = harness.picker.rescan(false);
    assert!(harness.picker.coverage().discovery_incomplete);
    harness.buffer(60, 20);
    harness.drive(Some(command));
    harness.press(&["enter", "v"]);
    assert_eq!(harness.picker.view, View::Browse);
    assert!(harness.launched().is_empty());
}

#[test]
fn attention_switch_and_status_removal_keep_the_connected_terminal_and_focus() {
    let mut harness = terminal_harness(&["/bin/sh", "-c", "sleep 60"]);
    harness.press(&["v"]);
    assert_eq!(harness.picker.view, View::Browse);
    assert!(harness.picker.terminal_focused);
    let active_started = harness.picker.active.as_ref().unwrap().started;
    let computer = harness.picker.entries[0].worker.clone();
    let waiting = with_agent(session("$0", "work"), AgentState::Waiting, None);
    harness.picker.update(Message::AgentStatuses {
        generation: harness.picker.generation,
        results: vec![(computer.clone(), Ok(status_rows(&[waiting])))],
    });
    let area = harness.picker.view_area;
    harness.picker.mouse(MouseEvent {
        kind: MouseEventKind::Down(event::MouseButton::Left),
        column: area.x,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(harness.picker.view, View::Attention);
    assert!(harness.picker.terminal_focused);
    harness.picker.update(Message::AgentStatuses {
        generation: harness.picker.generation,
        results: vec![(computer, Ok(Vec::new()))],
    });
    assert!(harness.picker.current_row().is_none());
    harness.buffer(120, 30);
    assert!(harness.picker.terminal_visible && harness.picker.terminal_focused);
    assert_eq!(
        harness.picker.active.as_ref().unwrap().started,
        active_started
    );
    harness
        .picker
        .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness.press(&["v"]);
    assert_eq!(harness.picker.view, View::Browse);
    assert_eq!(
        harness.picker.active.as_ref().unwrap().started,
        active_started
    );
    harness.press(&["/", "v"]);
    assert_eq!(harness.picker.view, View::Browse);
    assert_eq!(harness.picker.filter, "v");
    assert!(harness.launched().is_empty());
}

#[test]
fn attention_polls_unshared_remotes_in_browse_and_recovers_without_switching_views() {
    let computer = remote("remote", "remote", "remote.example");
    let waiting = with_agent(session("$1", "waiting"), AgentState::Waiting, None);
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![waiting.clone()])],
    );
    harness.picker.services.sharing_active = Box::new(|_| false);
    harness.picker.services.agent_status = Arc::new(|_, _| Err("connection failed".into()));
    let start = Instant::now();
    harness.tick(start);
    let next = start + STATUS_REFRESH;
    let command = harness.picker.update(Message::Tick(next));
    assert!(matches!(command, Some(Command::AgentStatuses { .. })));
    assert_eq!(harness.picker.coverage().checking, 1);
    assert!(harness.picker.update(Message::Tick(next)).is_none());
    harness.drive(command);
    assert_eq!(harness.effects.borrow().status_refreshes, ["remote"]);
    assert_eq!(harness.picker.coverage().checking, 0);
    assert_eq!(harness.picker.coverage().unavailable, 1);
    assert!(
        !harness
            .picker
            .rows
            .iter()
            .any(|row| row.kind == RowKind::NeedsInputHeader)
    );
    assert!(harness.launched().is_empty());

    harness.picker.services.agent_status =
        Arc::new(move |_, _| Ok(status_rows(std::slice::from_ref(&waiting))));
    harness.tick(next + STATUS_REFRESH);
    assert_eq!(harness.picker.view, View::Browse);
    assert_eq!(harness.picker.coverage().checked, 1);
    assert_eq!(harness.picker.coverage().unavailable, 0);
    assert_eq!(harness.picker.attention_count(AgentState::Waiting), 1);
    assert!(harness.picker.rows[0].kind == RowKind::NeedsInputHeader);
    harness.press(&["v"]);
    harness.tick(next + STATUS_REFRESH * 2);
    harness.press(&["v"]);
    harness.tick(next + STATUS_REFRESH * 3);
    assert_eq!(harness.effects.borrow().status_refreshes.len(), 4);
}

#[test]
fn attention_reused_session_ids_do_not_replace_the_selected_or_connected_lifetime() {
    for changed_field in 0..4 {
        let mut harness = terminal_harness(&["/bin/sh", "-c", "sleep 60"]);
        let computer = harness.picker.entries[0].worker.clone();
        let original = with_agent(session("$0", "work"), AgentState::Waiting, None);
        let other = with_agent(session("$1", "other"), AgentState::Waiting, None);
        harness.picker.update(Message::AgentStatuses {
            generation: harness.picker.generation,
            results: vec![(
                computer.clone(),
                Ok(status_rows(&[original.clone(), other.clone()])),
            )],
        });
        harness.picker.switch_view();
        let selection = harness.picker.selection.clone();
        let connected = harness.picker.active.as_ref().unwrap().started;
        let mut replacement = original.clone();
        match changed_field {
            0 => replacement.created += 1,
            1 => replacement.server_pid += 1,
            2 => replacement.server_start += 1,
            _ => replacement.socket_path.push_str("-replacement"),
        }
        harness.picker.update(Message::Checked(ready(
            &computer,
            vec![replacement.clone(), other],
        )));
        assert_eq!(
            attention_sessions(&harness),
            [("box".into(), "$1".into()), ("box".into(), "$0".into())]
        );
        assert_eq!(harness.picker.selection, selection);
        assert!(harness.picker.current_row().is_none());
        harness.buffer(120, 30);
        assert!(harness.picker.terminal_focused && harness.picker.terminal_visible);
        let active = harness.picker.active.as_ref().unwrap();
        assert_eq!(active.started, connected);
        assert!(same_session(active.session.as_ref().unwrap(), &original));
        harness
            .picker
            .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        harness.press(&["/", "w"]);
        assert!(harness.picker.current_row().is_none());
        assert_eq!(harness.picker.selection, selection);
        harness.press(&["esc"]);
        harness.press(&["enter", "v", "v", "enter"]);
        assert!(harness.launched().is_empty());
        assert_eq!(harness.picker.selection, selection);
        harness.press(&["down", "down", "enter"]);
        assert_eq!(harness.launched()[0].session.as_ref(), Some(&replacement));
        assert_eq!(harness.picker.active.as_ref().unwrap().started, connected);
    }
}

fn reporting_fixture(state: &str, action: Option<&str>) -> crate::reporting::Response {
    crate::reporting::parse(&serde_json::json!({
        "version": 1, "tmuxVersion": "3.8", "native": false, "detail": "Adapter reporting on tmux 3.8.",
        "targets": [{
            "id": "server-1-process-1", "sessionId": "$0", "sessionName": "work", "paneId": "%0",
            "agent": "omp", "profile": "wkchen", "state": state, "detail": "Profile reporter diagnosis.",
            "files": if action.is_some() { vec!["/home/u/.omp/profiles/wkchen/agent/extensions/delux-agent-status.ts"] } else { vec![] },
            "token": action.map(|_| "opaque target lifetime"), "action": action
        }]
    }).to_string(), false).unwrap()
}

fn reporting_open(harness: &mut Harness) -> (u64, Cancel, Worker) {
    let Command::ReportingSetup {
        epoch,
        cancel,
        workers,
        token,
    } = harness.picker.start_reporting_setup()
    else {
        panic!("expected inspection")
    };
    assert!(token.is_none());
    (epoch, cancel, workers[0].clone())
}

fn reporting_ready(harness: &mut Harness) -> (u64, Worker) {
    let (epoch, _, worker) = reporting_open(harness);
    assert!(
        harness
            .picker
            .update(Message::ReportingSetup {
                epoch,
                worker: worker.clone(),
                result: Ok(reporting_fixture("missing", Some("install"))),
            })
            .is_none()
    );
    (epoch, worker)
}

#[test]
fn reporting_setup_is_reachable_from_empty_attention_by_key_and_coverage_click() {
    let worker = worker("box", "box");
    let mut harness = Harness::new(vec![worker.clone()], vec![ready(&worker, vec![])]);
    harness.press(&["v"]);
    assert_eq!(harness.picker.attention_waiting, 0);
    let rendered = harness.rendered(100, 24);
    assert!(rendered.contains("s Set up reporting"));
    assert!(rendered.contains("s reporting setup"));
    let command = harness
        .picker
        .key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    let Some(Command::ReportingSetup { cancel, token, .. }) = command else {
        panic!("expected inspection")
    };
    assert!(token.is_none());
    assert!(harness.picker.reporting_setup.is_some());
    harness.press(&["esc"]);
    assert!(cancel.is_cancelled());
    harness.buffer(100, 24);
    let area = harness.picker.setup_area;
    let command = harness.picker.mouse(MouseEvent {
        kind: MouseEventKind::Down(event::MouseButton::Left),
        column: area.x,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    });
    assert!(matches!(
        command,
        Some(Command::ReportingSetup { token: None, .. })
    ));
}

#[test]
fn reporting_setup_install_requires_exact_target_confirmation_and_never_retries() {
    let worker = remote("box", "remote box", "user@box");
    let mut harness = Harness::new(
        vec![worker.clone()],
        vec![ready(&worker, vec![session("$0", "work")])],
    );
    reporting_ready(&mut harness);
    harness.press(&["y", "enter", "down", "enter"]);
    let rendered = harness.rendered(110, 30);
    assert!(rendered.contains("Confirm reporting changes"));
    assert!(rendered.contains("remote box (user@box)"));
    assert!(rendered.contains("profile: wkchen"));
    assert!(
        rendered.contains("/home/u/.omp/profiles/wkchen/agent/extensions/delux-agent-status.ts")
    );
    assert!(rendered.contains("No agent restart"));
    assert!(
        harness
            .picker
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .is_none()
    );
    let Some(Command::ReportingSetup {
        epoch,
        token,
        workers,
        ..
    }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
    else {
        panic!("expected confirmed install")
    };
    assert_eq!(token.as_deref(), Some("opaque target lifetime"));
    assert_eq!(workers, vec![worker.clone()]);
    assert!(
        harness
            .picker
            .key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .is_none()
    );
    assert!(
        harness
            .picker
            .key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .is_none()
    );
    assert!(
        harness
            .picker
            .update(Message::ReportingSetup {
                epoch,
                worker,
                result: Err("timeout".into())
            })
            .is_none()
    );
    let rendered = harness.rendered(110, 30);
    assert!(rendered.contains("Installation outcome uncertain"));
    assert!(rendered.contains("Do not retry"));
    harness.press(&["enter", "y", "enter"]);
    assert!(harness.launched().is_empty());
    assert!(matches!(
        harness
            .picker
            .key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
        Some(Command::ReportingSetup { token: None, .. })
    ));
}

#[test]
fn reporting_setup_rejects_closed_rechecked_and_retargeted_responses() {
    let worker = remote("box", "box", "old@box");
    let mut harness = Harness::new(
        vec![worker.clone()],
        vec![ready(&worker, vec![session("$0", "work")])],
    );
    let (old_epoch, old_cancel, _) = reporting_open(&mut harness);
    harness.press(&["esc"]);
    assert!(old_cancel.is_cancelled());
    harness.picker.update(Message::ReportingSetup {
        epoch: old_epoch,
        worker: worker.clone(),
        result: Ok(reporting_fixture("missing", Some("install"))),
    });
    assert!(harness.picker.reporting_setup.is_none());
    let (epoch, _, _) = reporting_open(&mut harness);
    harness.picker.update(Message::ReportingSetup {
        epoch: old_epoch,
        worker: worker.clone(),
        result: Ok(reporting_fixture("missing", Some("install"))),
    });
    assert!(!harness.rendered(110, 30).contains("wkchen"));
    harness.picker.entries[0].worker.destination = Some("new@box".into());
    harness.picker.update(Message::ReportingSetup {
        epoch,
        worker,
        result: Ok(reporting_fixture("missing", Some("install"))),
    });
    let rendered = harness.rendered(110, 30);
    assert!(rendered.contains("Computer/session changed"));
    assert!(!rendered.contains("wkchen"));
}

#[test]
fn reporting_setup_confirmation_rejects_each_session_lifetime_change() {
    for changed_field in 0..5 {
        let worker = worker("box", "box");
        let mut harness = Harness::new(
            vec![worker.clone()],
            vec![ready(&worker, vec![session("$0", "work")])],
        );
        reporting_ready(&mut harness);
        harness.press(&["down", "enter"]);
        let inventory = harness.picker.entries[0].inventory.as_mut().unwrap();
        match changed_field {
            0 => inventory.sessions[0].created += 1,
            1 => inventory.sessions[0].server_pid += 1,
            2 => inventory.sessions[0].server_start += 1,
            3 => inventory.sessions[0].socket_path.push_str("-new"),
            _ => inventory.sessions.clear(),
        }
        assert!(
            harness
                .picker
                .key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
                .is_none()
        );
        assert!(harness.rendered(110, 30).contains("nothing was installed"));
    }
}

#[test]
fn reporting_setup_inspection_and_confirmation_do_not_replace_or_refocus_terminal() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let connected = harness.picker.active.as_ref().unwrap().started;
    let selection = harness.picker.selection.clone();
    harness.picker.view = View::Attention;
    harness.buffer(120, 30);
    let area = harness.picker.setup_area;
    let Some(Command::ReportingSetup { epoch, workers, .. }) = harness.picker.mouse(MouseEvent {
        kind: MouseEventKind::Down(event::MouseButton::Left),
        column: area.x,
        row: area.y,
        modifiers: KeyModifiers::NONE,
    }) else {
        panic!("expected coverage setup")
    };
    harness.picker.update(Message::ReportingSetup {
        epoch,
        worker: workers[0].clone(),
        result: Ok(reporting_fixture("missing", Some("install"))),
    });
    assert!(harness.picker.terminal_focused);
    harness.press(&["down", "enter"]);
    assert!(
        harness
            .rendered(120, 30)
            .contains("Confirm reporting changes")
    );
    assert!(harness.picker.paste("do not send to agent").is_none());
    harness.press(&["esc", "esc"]);
    assert_eq!(harness.picker.selection, selection);
    assert_eq!(harness.picker.active.as_ref().unwrap().started, connected);
    assert!(harness.picker.terminal_focused);
    harness.buffer(120, 30);
    assert!(harness.picker.terminal_visible);
    assert!(harness.launched().is_empty());
}

#[test]
fn reporting_setup_distinguishes_installed_native_unknown_and_actual_reporting() {
    let worker = worker("box", "box");
    for (state, label) in [
        ("reporting", "reporting"),
        ("native", "native"),
        ("installed", "installed · reporting not confirmed"),
        ("unknown", "unknown"),
        ("conflict", "conflict · not modified"),
    ] {
        let mut harness = Harness::new(
            vec![worker.clone()],
            vec![ready(&worker, vec![session("$0", "work")])],
        );
        let (epoch, _, _) = reporting_open(&mut harness);
        harness.picker.update(Message::ReportingSetup {
            epoch,
            worker: worker.clone(),
            result: Ok(reporting_fixture(state, None)),
        });
        assert!(harness.rendered(120, 30).contains(label), "{state}");
        harness.press(&["down", "enter", "y"]);
        assert!(
            !harness
                .rendered(120, 30)
                .contains("Confirm reporting changes")
        );
    }
}

#[test]
fn reporting_setup_success_preserves_terminal_and_does_not_fabricate_agent_status() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let connected = harness.picker.active.as_ref().unwrap().started;
    let before = harness.picker.entries[0].inventory.clone();
    let (_, worker) = reporting_ready(&mut harness);
    harness.press(&["down", "enter"]);
    let Some(Command::ReportingSetup { epoch, .. }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
    else {
        panic!("expected install")
    };
    let installed = crate::reporting::parse(r#"{"version":1,"status":"installed","files":["/home/u/.omp/profiles/wkchen/agent/extensions/delux-agent-status.ts"],"detail":"Installed. Restart/resume at a safe point."}"#, true).unwrap();
    assert!(
        harness
            .picker
            .update(Message::ReportingSetup {
                epoch,
                worker,
                result: Ok(installed)
            })
            .is_none()
    );
    let rendered = harness.rendered(120, 30);
    assert!(rendered.contains("installed · reporting not confirmed"));
    assert!(rendered.contains("Restart/resume"));
    assert_eq!(harness.picker.entries[0].inventory, before);
    assert_eq!(harness.picker.active.as_ref().unwrap().started, connected);
    assert!(harness.picker.terminal_focused);
    harness.press(&["enter", "y", "esc"]);
    assert!(harness.launched().is_empty());
    assert!(harness.picker.reporting_setup.is_none());
}

#[test]
fn reporting_setup_cancelled_install_needs_fresh_inspection_and_ignores_late_success() {
    let worker = worker("box", "box");
    let mut harness = Harness::new(
        vec![worker.clone()],
        vec![ready(&worker, vec![session("$0", "work")])],
    );
    reporting_ready(&mut harness);
    harness.press(&["down", "enter"]);
    let Some(Command::ReportingSetup { epoch, cancel, .. }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
    else {
        panic!("expected install")
    };
    harness.press(&["esc"]);
    assert!(cancel.is_cancelled());
    let (new_epoch, _, _) = reporting_open(&mut harness);
    assert_ne!(epoch, new_epoch);
    let installed = crate::reporting::parse(
        r#"{"version":1,"status":"installed","files":["/tmp/adapter"],"detail":"Old success"}"#,
        true,
    )
    .unwrap();
    harness.picker.update(Message::ReportingSetup {
        epoch,
        worker,
        result: Ok(installed),
    });
    let rendered = harness.rendered(110, 30);
    assert!(rendered.contains("checking"));
    assert!(!rendered.contains("Old success"));
    harness.press(&["down", "enter", "y"]);
    assert!(
        !harness
            .rendered(110, 30)
            .contains("Confirm reporting changes")
    );
}

#[test]
fn browse_attention_shortcuts_match_visible_targets_and_respect_filters() {
    let alpha = worker("alpha", "alpha-host");
    let beta = remote("beta", "beta-host", "beta.example");
    let sessions: Vec<_> = (1..=5)
        .map(|n| {
            with_agent(
                session(&format!("${n}"), &format!("review-{n}")),
                AgentState::Waiting,
                None,
            )
        })
        .collect();
    let mut harness = Harness::new(
        vec![alpha.clone(), beta.clone()],
        vec![
            ready(&alpha, sessions.clone()),
            ready(&beta, sessions.clone()),
        ],
    );
    let targets: Vec<_> = harness
        .picker
        .rows
        .iter()
        .filter(|row| row.shortcut.is_some())
        .map(|row| {
            (
                row.shortcut.unwrap(),
                harness.picker.entries[row.entry].worker.clone(),
                harness.picker.session(*row).unwrap().clone(),
            )
        })
        .collect();
    assert_eq!(targets.len(), 9);
    assert!(harness.picker.rows[0].kind == RowKind::NeedsInputHeader);
    for (index, (number, worker, session)) in targets.into_iter().enumerate() {
        assert_eq!(number as usize, index + 1);
        harness.press(&[&number.to_string()]);
        let launched = harness.launched();
        assert_eq!(launched.last().unwrap().worker, worker);
        assert_eq!(launched.last().unwrap().session.as_ref(), Some(&session));
    }
    harness.press(&["v"]);
    assert_eq!(attention_sessions(&harness).len(), 10);
    harness.press(&["v", "/", "beta-host", "enter"]);
    assert_eq!(
        harness
            .picker
            .rows
            .iter()
            .filter(|row| row.shortcut.is_some())
            .count(),
        5
    );
    harness.press(&["1"]);
    assert_eq!(harness.launched().last().unwrap().worker, beta);
    let launches = harness.launched().len();
    harness.press(&["9"]);
    assert_eq!(harness.launched().len(), launches);
    harness.press(&["/", "1"]);
    assert_eq!(harness.picker.filter, "beta-host1");
    assert_eq!(harness.launched().len(), launches);
    assert!(
        !harness
            .picker
            .rows
            .iter()
            .any(|row| row.kind == RowKind::NeedsInputHeader)
    );
}

#[test]
fn browse_attention_removal_hides_empty_section_without_retargeting_enter() {
    let computer = worker("box", "box");
    let waiting = with_agent(session("$1", "review"), AgentState::Waiting, None);
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(
            &computer,
            vec![waiting.clone(), session("$2", "other")],
        )],
    );
    harness.press(&["1"]);
    let selected = harness.picker.selection.clone();
    harness.picker.update(Message::AgentStatuses {
        generation: harness.picker.generation,
        results: vec![(
            computer,
            Ok(status_rows(&[with_agent(waiting, AgentState::Done, None)])),
        )],
    });
    assert_eq!(harness.picker.selection, selected);
    assert!(harness.picker.current_row().is_none());
    harness.press(&["enter", "1"]);
    assert_eq!(harness.launched().len(), 1);
    assert!(harness.picker.rows[0].kind == RowKind::ComputersHeader);
    assert!(!harness.picker.rows.iter().any(|row| matches!(
        row.kind,
        RowKind::NeedsInputHeader | RowKind::AttentionSession
    )));
}

#[test]
fn directory_completion_does_not_overwrite_edits_or_disconnect_the_client() {
    let mut harness = terminal_harness(&["/bin/cat"]);
    let started = harness.picker.active.as_ref().unwrap().started;
    harness
        .picker
        .key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness.press(&["n", "new-work", "tab"]);
    let Some(Command::CompleteDirectory {
        epoch,
        worker,
        path,
        cancel,
    }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
    else {
        panic!("expected directory lookup");
    };
    harness.press(&["x", "backspace"]);
    assert!(cancel.is_cancelled());
    harness.picker.update(Message::DirectoryCompleted {
        epoch,
        worker,
        path,
        result: Ok(crate::directory_completion::DirectoryCompletion {
            path: "~/".into(),
            exists: true,
            matches: vec!["~/unwanted/".into()],
            truncated: false,
        }),
    });
    let Some(Command::Create { request, .. }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("expected explicit creation");
    };
    assert_eq!(request.directory, "~");
    assert_eq!(harness.picker.active.as_ref().unwrap().started, started);
    assert!(harness.launched().is_empty());
}

#[test]
fn directory_completion_reopened_form_rejects_old_reply_and_uses_exact_remote_path() {
    let computer = remote("box", "box", "alice@box");
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![session("$0", "work")])],
    );
    harness.press(&["n", "old-work", "tab"]);
    let Some(Command::CompleteDirectory {
        epoch: old_epoch,
        cancel: old_cancel,
        ..
    }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
    else {
        panic!("expected first lookup");
    };
    harness.press(&["esc", "n", "new-work", "tab"]);
    assert!(old_cancel.is_cancelled());
    let Some(Command::CompleteDirectory {
        epoch,
        worker,
        path,
        cancel,
    }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
    else {
        panic!("expected second lookup");
    };
    assert_eq!(worker, computer);
    harness.picker.update(Message::DirectoryCompleted {
        epoch: old_epoch,
        worker: worker.clone(),
        path: path.clone(),
        result: Err("old connection failed".into()),
    });
    assert!(!cancel.is_cancelled());
    assert!(
        harness
            .picker
            .creation
            .as_ref()
            .unwrap()
            .directory_completion_pending()
    );
    harness.picker.update(Message::DirectoryCompleted {
        epoch,
        worker,
        path,
        result: Ok(crate::directory_completion::DirectoryCompletion {
            path: "~/".into(),
            exists: true,
            matches: vec!["~/Projects with spaces/".into()],
            truncated: false,
        }),
    });
    let Some(Command::Create {
        worker, request, ..
    }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("expected explicit creation");
    };
    assert_eq!(worker, computer);
    assert_eq!(request.name, "new-work");
    assert_eq!(request.directory, "~/Projects with spaces/");
    assert!(harness.launched().is_empty());
}

#[test]
fn directory_completion_rejects_a_changed_computer() {
    let computer = remote("box", "box", "alice@box");
    let mut harness = Harness::new(
        vec![computer.clone()],
        vec![ready(&computer, vec![session("$0", "work")])],
    );
    harness.press(&["n", "new-work", "tab"]);
    let Some(Command::CompleteDirectory {
        epoch,
        worker,
        path,
        ..
    }) = harness
        .picker
        .key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
    else {
        panic!("expected lookup");
    };
    harness.picker.entries[0].worker.destination = Some("bob@box".into());
    harness.picker.update(Message::DirectoryCompleted {
        epoch,
        worker,
        path,
        result: Ok(crate::directory_completion::DirectoryCompletion {
            path: "~/".into(),
            exists: true,
            matches: vec!["~/wrong-account/".into()],
            truncated: false,
        }),
    });
    assert!(
        !harness
            .picker
            .creation
            .as_ref()
            .unwrap()
            .directory_completion_pending()
    );
    harness.press(&["enter"]);
    assert!(harness.picker.creation_pending.is_none());
    assert!(harness.launched().is_empty());
}
