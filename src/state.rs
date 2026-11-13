use crate::files::{FILE_LIMIT, read_utf8, xdg_path};
use serde::{Deserialize, Serialize};
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub pins: Vec<Pin>,
    pub recent: Vec<Recent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    #[serde(rename = "computer")]
    pub worker: String,
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recent {
    #[serde(rename = "computer")]
    pub worker: String,
    pub session: String,
}

pub const RECENT_LIMIT: usize = 10;

#[derive(Clone, Copy)]
pub(crate) enum StateChange<'a> {
    TogglePin { worker: &'a str, session: &'a str },
    RecordRecent { worker: &'a str, session: &'a str },
}

/// Uses XDG_STATE_HOME, falling back to ~/.local/state.
pub fn default_state_path() -> Result<String, String> {
    xdg_path("XDG_STATE_HOME", ".local/state", "state.json")
}

/// Missing state is empty; malformed or oversized state is never silently discarded.
pub fn load_state(path: &str) -> Result<State, String> {
    read_state(path).map_err(|error| {
        format!("{path}: unreadable picker state ({error}); move it aside to reset pins")
    })
}

fn read_state(path: &str) -> Result<State, String> {
    let Some(raw) = read_utf8(path, "Picker state")? else {
        return Ok(State::default());
    };
    let fields: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&raw).map_err(|error| format!("Invalid picker state: {error}"))?;
    for name in ["pins", "recent"] {
        if !fields.get(name).is_some_and(|value| {
            value
                .as_array()
                .is_some_and(|entries| entries.iter().all(serde_json::Value::is_object))
        }) {
            return Err(format!("Picker state '{name}' must be an array of objects"));
        }
    }
    serde_json::from_value(serde_json::Value::Object(fields))
        .map_err(|error| format!("Invalid picker state: {error}"))
}

/// Applies one operation to fresh state under a stable, cross-process lock.
pub(crate) fn update_state(path: &str, change: StateChange<'_>) -> Result<State, String> {
    locked_update(path, |target| {
        let mut state = load_state(target)?;
        state.apply(change);
        let mut raw = serde_json::to_vec_pretty(&state)
            .map_err(|error| format!("Serialize picker state: {error}"))?;
        raw.push(b'\n');
        Ok((state, raw))
    })
}

/// Shared by picker state and CLI host mappings: `update` reads the resolved target fresh
/// and returns its replacement while a persistent `.lock` beside that target is held, so
/// independent writers never lose each other's changes or expose a partial file.
/// Reading the resolved target, not `path`, keeps a symlink repointed meanwhile from
/// mixing one file's contents into another.
pub(crate) fn locked_update<T>(
    path: &str,
    update: impl FnOnce(&str) -> Result<(T, Vec<u8>), String>,
) -> Result<T, String> {
    let target = write_target(Path::new(path))?;
    let target_path = target
        .to_str()
        .ok_or_else(|| format!("{} is not valid UTF-8", target.display()))?;
    create_parent(&target)?;
    let mut lock_path = target.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    // The lock file is never renamed or removed: all writers must lock the same inode.
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|error| format!("open {}: {error}", lock_path.display()))?;
    loop {
        // SAFETY: lock owns this descriptor until the read-modify-write completes.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("lock {}: {error}", lock_path.display()));
        }
    }
    let (value, raw) = update(target_path)?;
    atomic_write_target(&target, &raw)?;
    Ok(value)
}

/// Preserve existing symlinks; dangling or unresolvable links are not safe to replace.
fn write_target(path: &Path) -> Result<PathBuf, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::canonicalize(path)
            .map_err(|error| format!("resolve {}: {error}", path.display())),
        Ok(_) => Ok(path.to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_owned()),
        Err(error) => Err(format!("inspect {}: {error}", path.display())),
    }
}

fn create_parent(target: &Path) -> Result<&Path, String> {
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;
    Ok(parent)
}

fn atomic_write_target(target: &Path, raw: &[u8]) -> Result<(), String> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    if raw.len() > FILE_LIMIT {
        return Err(format!(
            "{} would exceed 64 KiB; nothing was written",
            target.display()
        ));
    }
    let parent = create_parent(target)?;
    let (temp, mut file) = loop {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".delux-{}-{sequence}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
        {
            Ok(file) => break (temp, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("create {}: {error}", temp.display())),
        }
    };
    let result = (|| {
        file.write_all(raw)
            .map_err(|error| format!("write {}: {error}", temp.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync {}: {error}", temp.display()))?;
        std::fs::rename(&temp, target)
            .map_err(|error| format!("rename {}: {error}", target.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

impl State {
    pub(crate) fn apply(&mut self, change: StateChange<'_>) {
        match change {
            StateChange::TogglePin { worker, session } => {
                self.toggle_pin(worker, session);
            }
            StateChange::RecordRecent { worker, session } => self.record_recent(worker, session),
        }
    }

    pub fn is_pinned(&self, worker: &str, session: &str) -> bool {
        self.pins
            .iter()
            .any(|pin| pin.worker == worker && pin.session == session)
    }

    /// Toggles one computer/session-name pair, preserving insertion order.
    pub fn toggle_pin(&mut self, worker: &str, session: &str) -> bool {
        if self.is_pinned(worker, session) {
            self.pins
                .retain(|pin| pin.worker != worker || pin.session != session);
            false
        } else {
            self.pins.push(Pin {
                worker: worker.into(),
                session: session.into(),
            });
            true
        }
    }

    /// Moves the pair to the front, deduplicates, and retains at most ten entries.
    pub fn record_recent(&mut self, worker: &str, session: &str) {
        let recent = match self
            .recent
            .iter()
            .position(|recent| recent.worker == worker && recent.session == session)
        {
            Some(index) => self.recent.remove(index),
            None => Recent {
                worker: worker.into(),
                session: session.into(),
            },
        };
        self.recent.insert(0, recent);
        let mut index = 1;
        while index < self.recent.len() && index < RECENT_LIMIT {
            let candidate = &self.recent[index];
            if self.recent[..index].iter().any(|recent| {
                recent.worker == candidate.worker && recent.session == candidate.session
            }) {
                self.recent.remove(index);
            } else {
                index += 1;
            }
        }
        self.recent.truncate(RECENT_LIMIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EnvVar, TempDir, env_read, env_write};
    use std::io::Read;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::{Command, Stdio};

    #[test]
    fn default_path_uses_xdg_then_home() {
        let _env = env_write();
        let home = TempDir::new();
        let xdg = home.path().join("xdg");
        let _home = EnvVar::set("HOME", home.path());
        let _xdg = EnvVar::set("XDG_STATE_HOME", &xdg);
        assert_eq!(
            default_state_path().unwrap(),
            xdg.join("delux/state.json").to_str().unwrap()
        );
        let _empty_xdg = EnvVar::set("XDG_STATE_HOME", "");
        assert_eq!(
            default_state_path().unwrap(),
            home.path()
                .join(".local/state/delux/state.json")
                .to_str()
                .unwrap()
        );
        let _no_xdg = EnvVar::remove("XDG_STATE_HOME");
        assert_eq!(
            default_state_path().unwrap(),
            home.path()
                .join(".local/state/delux/state.json")
                .to_str()
                .unwrap()
        );
        let _no_home = EnvVar::remove("HOME");
        assert!(default_state_path().is_err());
        let _xdg_without_home = EnvVar::set("XDG_STATE_HOME", &xdg);
        assert_eq!(
            default_state_path().unwrap(),
            xdg.join("delux/state.json").to_str().unwrap()
        );
    }

    #[test]
    fn state_rejects_invalid_shapes_and_accepts_bounded_utf8() {
        let dir = TempDir::new();
        let path = dir.path().join("state.json");
        let path = path.to_str().unwrap();
        assert_eq!(load_state(path).unwrap(), State::default());
        let mut invalid: Vec<Vec<u8>> = [
            "",
            "null",
            "[]",
            "[[],[]]",
            "42",
            "{} {}",
            r#"{"pins":[],"recent":[],"unknown":true}"#,
            r#"{"pins":{},"recent":[]}"#,
            r#"{"pins":null,"recent":[]}"#,
            r#"{"pins":[],"recent":null}"#,
            r#"{"pins":[{"computer":"example","session":"work","unknown":true}],"recent":[]}"#,
            r#"{"pins":[],"recent":[{"computer":"example","session":"work","unknown":true}]}"#,
            r#"{"pins":[],"recent":[{"computer":"example","session":"work","at":1}]}"#,
            r#"{"pins":[["example","work"]],"recent":[]}"#,
            r#"{"pins":[],"recent":[["example","work"]]}"#,
            r#"{"pins":[{"computer":null,"session":"work"}],"recent":[]}"#,
            r#"{"pins":[],"recent":[{"computer":"example","session":null}]}"#,
            r#"{"favorites":[],"recent":[]}"#,
            r#"{"pins":[{"worker":"example","session":"work"}],"recent":[]}"#,
            r#"{"pins":[],"recent":[{"worker":"example","session":"work"}]}"#,
        ]
        .iter()
        .map(|raw| raw.as_bytes().to_vec())
        .collect();
        invalid.push(b"{\"pins\":[\"\xff\"],\"recent\":[]}".to_vec());
        invalid.push(format!("{}{{}}", " ".repeat(64 * 1024)).into_bytes());
        for (index, raw) in invalid.iter().enumerate() {
            std::fs::write(path, raw).unwrap();
            assert!(
                load_state(path).is_err(),
                "accepted invalid state case {index}"
            );
        }
        let raw = r#"{"pins":[],"recent":[]}"#;
        std::fs::write(path, format!("{raw}{}", " ".repeat(64 * 1024 - raw.len()))).unwrap();
        assert_eq!(load_state(path).unwrap(), State::default());
    }

    #[test]
    fn state_round_trip_is_private_and_atomic() {
        let dir = TempDir::new();
        let parent = dir.path().join("nested/delux");
        let path = parent.join("state.json");
        let path = path.to_str().unwrap();
        update_state(
            path,
            StateChange::TogglePin {
                worker: "local",
                session: "work name",
            },
        )
        .unwrap();
        let state = update_state(
            path,
            StateChange::RecordRecent {
                worker: "remote",
                session: "日本語",
            },
        )
        .unwrap();
        assert_eq!(load_state(path).unwrap(), state);
        let serialized: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            serialized,
            serde_json::json!({
                "pins": [{"computer": "local", "session": "work name"}],
                "recent": [{"computer": "remote", "session": "日本語"}],
            }),
        );
        assert!(std::fs::read(path).unwrap().ends_with(b"\n"));
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for directory in [&parent, &dir.path().join("nested")] {
            assert_eq!(
                std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let state = update_state(
            path,
            StateChange::RecordRecent {
                worker: "local",
                session: "work name",
            },
        )
        .unwrap();
        assert_eq!(load_state(path).unwrap(), state);
    }

    #[test]
    fn failed_atomic_write_removes_its_temporary_file() {
        let dir = TempDir::new();
        let target = dir.path().join("directory");
        std::fs::create_dir(&target).unwrap();
        assert!(atomic_write_target(&target, b"unused").is_err());
        assert!(target.is_dir());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn symlinked_configuration_and_state_update_the_destination() {
        let dir = TempDir::new();
        let stored = dir.path().join("stored");
        std::fs::create_dir(&stored).unwrap();
        for (filename, raw) in [
            ("cli.json", r#"{"hosts":{},"ignore":[]}"#),
            ("state.json", r#"{"pins":[],"recent":[]}"#),
        ] {
            let target = stored.join(filename);
            let link = dir.path().join(filename);
            let relative = Path::new("stored").join(filename);
            std::fs::write(&target, raw).unwrap();
            symlink(&relative, &link).unwrap();
            let mut original = std::fs::File::open(&target).unwrap();
            let path = link.to_str().unwrap();
            if filename == "cli.json" {
                crate::workers::save_host_mapping(path, "node", "alice@host").unwrap();
                let config = crate::workers::load_config(target.to_str().unwrap()).unwrap();
                assert_eq!(config.hosts["node"], "alice@host");
            } else {
                let state = update_state(
                    path,
                    StateChange::TogglePin {
                        worker: "node",
                        session: "main",
                    },
                )
                .unwrap();
                assert!(state.is_pinned("node", "main"));
                assert_eq!(load_state(target.to_str().unwrap()).unwrap(), state);
            }
            assert_eq!(std::fs::read_link(&link).unwrap(), relative);
            let mut old_contents = String::new();
            original.read_to_string(&mut old_contents).unwrap();
            assert_eq!(old_contents, raw);
        }
    }

    #[test]
    fn updates_refuse_unresolvable_symlinks() {
        let dir = TempDir::new();
        for (name, destination) in [("dangling", "missing"), ("loop", "loop")] {
            let link = dir.path().join(name);
            symlink(destination, &link).unwrap();
            let replaced = locked_update(link.to_str().unwrap(), |_| {
                Ok(((), b"replacement".to_vec()))
            });
            assert!(replaced.is_err());
            assert_eq!(std::fs::read_link(&link).unwrap(), Path::new(destination));
        }
        assert!(!dir.path().join("missing").exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn updates_keep_reading_and_writing_the_resolved_symlink_target() {
        let dir = TempDir::new();
        let first = dir.path().join("first.json");
        let second = dir.path().join("second.json");
        let first_raw = r#"{"pins":[{"computer":"first","session":"main"}],"recent":[]}"#;
        let second_raw = r#"{"pins":[{"computer":"second","session":"main"}],"recent":[]}"#;
        std::fs::write(&first, first_raw).unwrap();
        std::fs::write(&second, second_raw).unwrap();
        let link = dir.path().join("state.json");
        symlink("first.json", &link).unwrap();
        let state = locked_update(link.to_str().unwrap(), |target| {
            // Repoint after resolution but before reading: the transaction must
            // read and replace the original target, never mix the two files.
            let next = dir.path().join("next");
            symlink("second.json", &next).unwrap();
            std::fs::rename(&next, &link).unwrap();
            let mut state = load_state(target)?;
            state.toggle_pin("added", "main");
            let raw = serde_json::to_vec(&state).unwrap();
            Ok((state, raw))
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(&second).unwrap(), second_raw);
        let saved = load_state(first.to_str().unwrap()).unwrap();
        assert_eq!(saved, state);
        assert_eq!(saved.pins.len(), 2);
        assert!(saved.is_pinned("first", "main") && saved.is_pinned("added", "main"));
    }

    #[test]
    fn updates_refuse_state_that_became_unreadable() {
        let dir = TempDir::new();
        let path = dir.path().join("state.json");
        let path = path.to_str().unwrap();
        assert_eq!(load_state(path).unwrap(), State::default());
        let raw = r#"{"pins":"damaged","recent":[]}"#;
        std::fs::write(path, raw).unwrap();
        let change = StateChange::TogglePin {
            worker: "node",
            session: "main",
        };
        assert!(update_state(path, change).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
        std::fs::remove_file(path).unwrap();
        std::fs::create_dir(path).unwrap();
        assert!(update_state(path, change).is_err());
        assert!(Path::new(path).is_dir());
    }

    #[test]
    fn updates_never_write_state_that_loading_would_refuse() {
        let dir = TempDir::new();
        let path = dir.path().join("state.json");
        let path = path.to_str().unwrap();
        let pin = StateChange::TogglePin {
            worker: "node",
            session: "main",
        };
        let saved = update_state(path, pin).unwrap();
        let raw = std::fs::read(path).unwrap();
        let oversized = "x".repeat(FILE_LIMIT);
        let change = StateChange::TogglePin {
            worker: "node",
            session: &oversized,
        };
        assert!(update_state(path, change).is_err());
        assert_eq!(std::fs::read(path).unwrap(), raw);
        assert_eq!(load_state(path).unwrap(), saved);
        assert!(!update_state(path, pin).unwrap().is_pinned("node", "main"));
    }

    #[test]
    #[ignore = "subprocess fixture for concurrent_processes_preserve_pins_and_recents_through_symlinks"]
    fn concurrent_state_writer_process() {
        let _env = env_read();
        let path = std::env::var_os("DELUX_STATE_TEST_PATH").expect("writer fixture path");
        let worker = std::env::var("DELUX_STATE_TEST_WORKER").unwrap();
        std::io::stdin().read_exact(&mut [0]).unwrap();
        for index in 0..8 {
            update_state(
                path.to_str().unwrap(),
                StateChange::TogglePin {
                    worker: &worker,
                    session: &index.to_string(),
                },
            )
            .unwrap();
            update_state(
                path.to_str().unwrap(),
                StateChange::RecordRecent {
                    worker: &worker,
                    session: "main",
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn concurrent_processes_preserve_pins_and_recents_through_symlinks() {
        let _env = env_read();
        let dir = TempDir::new();
        let path = dir.path().join("state.json");
        update_state(
            path.to_str().unwrap(),
            StateChange::TogglePin {
                worker: "original",
                session: "main",
            },
        )
        .unwrap();
        let alias = dir.path().join("alias.json");
        symlink("state.json", &alias).unwrap();
        let mut children: Vec<_> = (0..4)
            .map(|index| {
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "state::tests::concurrent_state_writer_process",
                        "--test-threads=1",
                        "--ignored",
                    ])
                    .env(
                        "DELUX_STATE_TEST_PATH",
                        if index % 2 == 0 { &path } else { &alias },
                    )
                    .env("DELUX_STATE_TEST_WORKER", index.to_string())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in &mut children {
            child.stdin.take().unwrap().write_all(b"x").unwrap();
        }
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let state = load_state(path.to_str().unwrap()).unwrap();
        assert_eq!(state.pins.len(), 33);
        assert!(state.is_pinned("original", "main"));
        assert_eq!(state.recent.len(), 4);
        for index in 0..4 {
            let worker = index.to_string();
            for session in 0..8 {
                assert!(state.is_pinned(&worker, &session.to_string()));
            }
            assert!(
                state
                    .recent
                    .iter()
                    .any(|recent| { recent.worker == worker && recent.session == "main" })
            );
        }
        assert!(
            std::fs::symlink_metadata(alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn toggling_pins_preserves_insertion_order() {
        let mut state = State::default();
        assert!(!state.is_pinned("a", "one"));
        assert!(state.toggle_pin("a", "one"));
        assert!(state.toggle_pin("b", "one"));
        assert!(state.toggle_pin("a", "two"));
        assert!(state.is_pinned("a", "one"));
        assert!(!state.toggle_pin("a", "one"));
        assert!(!state.is_pinned("a", "one"));
        assert!(state.toggle_pin("a", "one"));
        let pairs: Vec<_> = state
            .pins
            .iter()
            .map(|pin| (pin.worker.as_str(), pin.session.as_str()))
            .collect();
        assert_eq!(pairs, [("b", "one"), ("a", "two"), ("a", "one")]);
        state.pins.push(state.pins[2].clone());
        assert!(!state.toggle_pin("a", "one"));
        assert_eq!(state.pins.len(), 2);
    }

    #[test]
    fn recents_move_to_front_deduplicate_and_cap() {
        let mut state = State::default();
        state.record_recent("a", "one");
        state.record_recent("b", "one");
        state.record_recent("a", "two");
        state.recent.push(state.recent[1].clone());
        state.recent.push(state.recent[2].clone());
        state.record_recent("a", "one");
        let entries: Vec<_> = state
            .recent
            .iter()
            .map(|recent| (recent.worker.as_str(), recent.session.as_str()))
            .collect();
        assert_eq!(entries, [("a", "one"), ("a", "two"), ("b", "one")]);
        for index in 0..RECENT_LIMIT + 2 {
            state.record_recent("local", &index.to_string());
        }
        assert_eq!(state.recent.len(), RECENT_LIMIT);
        assert_eq!(state.recent.first().unwrap().session, "11");
        assert_eq!(state.recent.last().unwrap().session, "2");
    }
}
