use std::collections::BTreeSet;
use std::fs::{self, DirBuilder};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::Duration;

use crate::cancel::Cancel;
use crate::process::run_command;

const PERSIST_SECONDS: u64 = 30;
// Leave room for OpenSSH's temporary socket suffix as well as the terminating
// NUL on platforms whose sockaddr_un.sun_path is only 104 bytes.
const SOCKET_PATH_LIMIT: usize = 80;
static MULTIPLEXER: Mutex<Option<Multiplexer>> = Mutex::new(None);
static ATTACHMENT: Mutex<()> = Mutex::new(());

/// One interactive client runs at a time; background timeouts must not close
/// its shared SSH connection. Retain this guard until the client is cleaned up.
pub(crate) struct AttachmentGuard {
    _lock: MutexGuard<'static, ()>,
}

pub(crate) fn begin_attachment() -> AttachmentGuard {
    AttachmentGuard {
        _lock: ATTACHMENT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    }
}

struct Multiplexer {
    directory: Option<PathBuf>,
    destinations: BTreeSet<String>,
}

impl Multiplexer {
    fn new() -> Self {
        Self::for_process(
            std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
            std::process::id(),
        )
    }

    fn for_process(runtime: Option<&std::ffi::OsStr>, pid: u32) -> Self {
        let uid = current_uid();
        let base = directory_path(runtime, uid, pid);
        let directory = if prepare_directory(&base, uid) {
            sweep_stale_directories(&base, uid);
            let directory = base.join(pid.to_string());
            prepare_directory(&directory, uid).then_some(directory)
        } else {
            None
        };
        Self {
            directory,
            destinations: BTreeSet::new(),
        }
    }

    fn add_options(&mut self, args: &mut Vec<String>, destination: &str) {
        let Some(directory) = &self.directory else {
            return;
        };
        // Recheck before use, without repairing a directory another process has
        // changed. A rejected directory disables multiplexing for this run.
        if !safe_control_directory(directory, current_uid()) {
            self.directory = None;
            return;
        }
        args.extend([
            "-o".to_owned(),
            "ControlMaster=auto".to_owned(),
            "-o".to_owned(),
            control_path(directory),
            "-o".to_owned(),
            format!("ControlPersist={PERSIST_SECONDS}s"),
            "-o".to_owned(),
            "ServerAliveInterval=5".to_owned(),
            "-o".to_owned(),
            "ServerAliveCountMax=2".to_owned(),
        ]);
        if !self.destinations.contains(destination) {
            self.destinations.insert(destination.to_owned());
        }
    }

    fn sharing_active(&self, destination: &str) -> bool {
        self.destinations.contains(destination)
            && self
                .directory
                .as_deref()
                .is_some_and(|directory| safe_control_directory(directory, current_uid()))
    }

    fn close(self) {
        let Some(directory) = self.directory else {
            return;
        };
        if !safe_control_directory(&directory, current_uid()) {
            return;
        }
        close_masters(&directory, self.destinations);
        remove_socket_directory(&directory, false);
    }
}

fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn directory_path(runtime: Option<&std::ffi::OsStr>, uid: u32, pid: u32) -> PathBuf {
    if let Some(runtime) = runtime {
        let directory = Path::new(runtime).join("delux");
        // ControlPath is parsed as an ssh_config value, including percent and
        // environment expansion. Use the short fallback for unrepresentable or
        // special paths instead of letting configuration syntax change it.
        let bytes = directory.as_os_str().as_bytes();
        if directory.is_absolute()
            && bytes.len() + 1 + pid.to_string().len() + 1 + 40 <= SOCKET_PATH_LIMIT
            && bytes.iter().all(|byte| {
                byte.is_ascii_graphic() && !matches!(byte, b'%' | b'$' | b'\'' | b'"' | b'\\')
            })
        {
            return directory;
        }
    }
    PathBuf::from(format!("/tmp/delux-{uid}"))
}

fn prepare_directory(directory: &Path, uid: u32) -> bool {
    match DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return false,
    }
    safe_directory(directory, uid)
}

fn safe_directory(directory: &Path, uid: u32) -> bool {
    fs::symlink_metadata(directory).is_ok_and(|metadata| {
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0
    })
}

fn safe_control_directory(directory: &Path, uid: u32) -> bool {
    safe_directory(directory, uid)
        && directory
            .parent()
            .is_some_and(|base| safe_directory(base, uid))
}

fn process_is_gone(pid: u32) -> bool {
    let Ok(raw_pid) = i32::try_from(pid) else {
        return false;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw_pid) else {
        return false;
    };
    rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH)
}

fn sweep_stale_directories(base: &Path, uid: u32) {
    if !safe_directory(base, uid) {
        return;
    }
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if name == pid.to_string() && process_is_gone(pid) && safe_directory(&entry.path(), uid) {
            remove_socket_directory(&entry.path(), true);
        }
    }
}

fn remove_socket_directory(directory: &Path, require_stale: bool) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut sockets = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return;
        };
        let Ok(kind) = entry.file_type() else {
            return;
        };
        let path = entry.path();
        if !kind.is_socket() || (require_stale && !socket_is_stale(&path)) {
            return;
        }
        sockets.push(path);
    }
    for socket in sockets {
        let _ = fs::remove_file(socket);
    }
    let _ = fs::remove_dir(directory);
}

fn socket_is_stale(path: &Path) -> bool {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType, connect, socket};

    // Never block startup on a live master's full accept queue. Darwin does
    // not support socket creation flags, so set them on the owned descriptor.
    let Ok(fd) = socket(AddressFamily::UNIX, SocketType::STREAM, None) else {
        return false;
    };
    if rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC).is_err()
        || rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK).is_err()
    {
        return false;
    }
    let Ok(address) = SocketAddrUnix::new(path) else {
        return false;
    };
    matches!(
        connect(&fd, &address),
        Err(rustix::io::Errno::CONNREFUSED | rustix::io::Errno::NOENT)
    )
}

fn control_path(directory: &Path) -> String {
    format!("ControlPath={}/%C", directory.display())
}

pub(crate) fn add_options(args: &mut Vec<String>, destination: &str) {
    MULTIPLEXER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(Multiplexer::new)
        .add_options(args, destination);
}

pub(crate) fn sharing_active(destination: &str) -> bool {
    MULTIPLEXER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|multiplexer| multiplexer.sharing_active(destination))
}

pub(crate) fn reset_destination(destination: &str) {
    // Hold through the bounded reset so a new attachment cannot race it.
    let _idle = match ATTACHMENT.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(error)) => error.into_inner(),
        Err(TryLockError::WouldBlock) => return,
    };
    let directory = MULTIPLEXER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|multiplexer| multiplexer.sharing_active(destination))
        .and_then(|multiplexer| multiplexer.directory.clone());
    if let Some(directory) = directory {
        let cancel = Cancel::from_flag(Arc::new(AtomicBool::new(false)));
        close_master(&cancel, &control_path(&directory), destination);
    }
}

pub(crate) fn begin() {
    *MULTIPLEXER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Called only after command producers have been cancelled. Closing all masters
/// concurrently shares one deadline; ControlPersist is the SIGKILL backstop.
pub(crate) fn close() {
    let multiplexer = MULTIPLEXER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .replace(Multiplexer {
            directory: None,
            destinations: BTreeSet::new(),
        });
    if let Some(multiplexer) = multiplexer {
        multiplexer.close();
    }
}

fn close_masters(directory: &Path, destinations: BTreeSet<String>) {
    // Every control operation has the same bounded duration.
    // Shutdown must work even when the application token was cancelled by a
    // signal. These local control operations never start a remote connection.
    let cancel = Cancel::from_flag(Arc::new(AtomicBool::new(false)));
    let path = control_path(directory);
    // One thread per master shares the deadline; run_command kills and reaps a
    // control process that outlives it.
    std::thread::scope(|scope| {
        for destination in destinations {
            let path = &path;
            let cancel = &cancel;
            scope.spawn(move || {
                close_master(cancel, path, &destination);
            });
        }
    });
}

fn close_master(cancel: &Cancel, path: &str, destination: &str) {
    let args = [
        "ssh",
        "-O",
        "exit",
        "-o",
        path,
        "-o",
        "ControlMaster=no",
        "-o",
        "BatchMode=yes",
        "--",
        destination,
    ]
    .map(str::to_owned);
    let _ = run_command(cancel, &args, Duration::from_secs(1), false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EnvVar, TempDir, env_read, env_write};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;
    use std::time::Instant;

    fn exited_pid() -> u32 {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn process_instances_have_distinct_sockets_and_do_not_remove_live_directories() {
        let root = TempDir::new();
        let mut first = Multiplexer::for_process(Some(root.path().as_os_str()), std::process::id());
        // SAFETY: getppid has no arguments or failure mode.
        let parent = unsafe { libc::getppid() } as u32;
        let mut second = Multiplexer::for_process(Some(root.path().as_os_str()), parent);
        let mut first_args = Vec::new();
        let mut second_args = Vec::new();
        first.add_options(&mut first_args, "host");
        second.add_options(&mut second_args, "host");
        let first_path = first_args
            .iter()
            .find(|arg| arg.starts_with("ControlPath="))
            .unwrap();
        let second_path = second_args
            .iter()
            .find(|arg| arg.starts_with("ControlPath="))
            .unwrap();
        assert_ne!(first_path, second_path);
        assert!(first_path.contains(&format!("/{}/%C", std::process::id())));
        assert!(second_path.contains(&format!("/{parent}/%C")));
        for directory in [first.directory.unwrap(), second.directory.unwrap()] {
            assert!(safe_control_directory(&directory, current_uid()));
            assert!(directory.as_os_str().len() + 41 <= SOCKET_PATH_LIMIT);
        }
    }

    #[test]
    fn stale_sweep_only_removes_dead_process_directories_without_live_sockets() {
        let _env = env_read();
        let root = TempDir::new();
        let uid = current_uid();
        let stale = root.path().join(exited_pid().to_string());
        assert!(prepare_directory(&stale, uid));
        let stale_socket = stale.join("stale");
        drop(UnixListener::bind(&stale_socket).unwrap());
        let live_socket = stale.join("live");
        let listener = UnixListener::bind(&live_socket).unwrap();
        let current = root.path().join(std::process::id().to_string());
        assert!(prepare_directory(&current, uid));
        let empty = root.path().join(exited_pid().to_string());
        assert!(prepare_directory(&empty, uid));
        let alias = root.path().join(exited_pid().to_string());
        symlink(&stale, &alias).unwrap();
        let unsafe_mode = root.path().join(exited_pid().to_string());
        assert!(prepare_directory(&unsafe_mode, uid));
        fs::set_permissions(&unsafe_mode, fs::Permissions::from_mode(0o755)).unwrap();
        sweep_stale_directories(root.path(), uid);
        assert!(stale.is_dir());
        assert!(stale_socket.exists());
        assert!(current.is_dir());
        assert!(!empty.exists());
        assert!(
            fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(unsafe_mode.is_dir());
        drop(listener);
        // Tests run in parallel: a child another test forked keeps a copy of
        // the listener until it execs, so the socket can look live briefly.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            sweep_stale_directories(root.path(), uid);
            if !stale.exists() {
                break;
            }
            assert!(Instant::now() < deadline, "stale directory survived");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(current.is_dir());
    }

    #[test]
    fn unsafe_base_or_process_permissions_disable_multiplexing_for_the_run() {
        let root = TempDir::new();
        let mut mux = Multiplexer::for_process(Some(root.path().as_os_str()), std::process::id());
        let directory = mux.directory.clone().unwrap();
        let base = directory.parent().unwrap();
        fs::set_permissions(base, fs::Permissions::from_mode(0o755)).unwrap();
        let mut args = Vec::new();
        mux.add_options(&mut args, "host");
        assert!(args.is_empty());
        fs::set_permissions(base, fs::Permissions::from_mode(0o700)).unwrap();
        mux.add_options(&mut args, "host");
        assert!(args.is_empty());
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o750)).unwrap();
        let mut mux = Multiplexer::for_process(Some(root.path().as_os_str()), std::process::id());
        mux.add_options(&mut args, "host");
        assert!(args.is_empty());
    }

    #[test]
    fn stuck_cleanup_commands_share_one_deadline_and_are_reaped() {
        let _env = env_write();
        let root = TempDir::new();
        let ssh = root.path().join("ssh");
        fs::write(
            &ssh,
            "#!/bin/sh\nfor destination do :; done\nprintf '%s\\n' \"$$\" > \"$MUX_LOG.$destination\"\nexec /bin/sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let _path = EnvVar::set("PATH", root.path());
        let log = root.path().join("calls");
        let _log = EnvVar::set("MUX_LOG", &log);
        let destinations = BTreeSet::from(["one".to_owned(), "two".to_owned(), "three".to_owned()]);
        let started = Instant::now();
        close_masters(root.path(), destinations);
        assert!(started.elapsed() < Duration::from_secs(3));
        for destination in ["one", "two", "three"] {
            let pid = fs::read_to_string(format!("{}.{destination}", log.display()))
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap();
            assert!(process_is_gone(pid));
        }
    }

    #[test]
    fn private_directory_rejects_links_permissions_and_wrong_owner() {
        let root = TempDir::new();
        let directory = root.path().join("delux");
        assert!(prepare_directory(&directory, current_uid()));
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
        assert!(!prepare_directory(
            &directory,
            current_uid().wrapping_add(1)
        ));
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(!prepare_directory(&directory, current_uid()));
        assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o750);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = root.path().join("alias");
        symlink(&directory, &alias).unwrap();
        assert!(!prepare_directory(&alias, current_uid()));
        let file = root.path().join("file");
        fs::write(&file, "").unwrap();
        assert!(!prepare_directory(&file, current_uid()));
    }

    #[test]
    fn socket_directory_prefers_short_absolute_runtime_paths() {
        assert_eq!(
            directory_path(Some("/run/user/123".as_ref()), 123, 456),
            Path::new("/run/user/123/delux")
        );
        for runtime in [
            None,
            Some("relative".as_ref()),
            Some("/a/very/long/runtime/directory/that/cannot/hold/a/unix/socket".as_ref()),
            Some("/tmp/%h".as_ref()),
        ] {
            let directory = directory_path(runtime, 123, 456);
            assert_eq!(directory, Path::new("/tmp/delux-123"));
            assert!(directory.as_os_str().len() + 4 + 41 <= SOCKET_PATH_LIMIT);
        }
    }

    #[test]
    fn cleanup_closes_each_used_destination_once_and_removes_process_directory() {
        let _env = env_write();
        let root = TempDir::new();
        let base = root.path().join("delux");
        assert!(prepare_directory(&base, current_uid()));
        let directory = base.join(std::process::id().to_string());
        assert!(prepare_directory(&directory, current_uid()));
        let log = root.path().join("calls");
        let ssh = root.path().join("ssh");
        fs::write(&ssh, "#!/bin/sh\nfor destination do :; done\nprintf '%s\\n' \"$@\" > \"$MUX_LOG.$destination\"\n").unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let _path = EnvVar::set("PATH", root.path());
        let _log = EnvVar::set("MUX_LOG", &log);
        let mut mux = Multiplexer {
            directory: Some(directory.clone()),
            destinations: BTreeSet::new(),
        };
        let mut first = Vec::new();
        mux.add_options(&mut first, "user@one");
        let mut later = Vec::new();
        mux.add_options(&mut later, "user@one");
        assert_eq!(first, later);
        mux.add_options(&mut Vec::new(), "two");
        mux.close();
        for destination in ["user@one", "two"] {
            let calls = fs::read_to_string(format!("{}.{destination}", log.display())).unwrap();
            assert_eq!(
                calls,
                format!(
                    "-O\nexit\n-o\n{}\n-o\nControlMaster=no\n-o\nBatchMode=yes\n--\n{destination}\n",
                    control_path(&directory)
                )
            );
        }
        assert!(!directory.exists());
        assert!(base.is_dir());
    }
}
