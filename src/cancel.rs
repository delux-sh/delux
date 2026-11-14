use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Scope {
    cancelled: Arc<AtomicBool>,
    parent: Option<Arc<Scope>>,
}

impl Scope {
    fn descends_from(&self, ancestor: &Self) -> bool {
        let mut scope = Some(self);
        while let Some(current) = scope {
            if std::ptr::eq(current, ancestor) {
                return true;
            }
            scope = current.parent.as_deref();
        }
        false
    }
}

#[derive(Debug)]
struct Process {
    pid: u32,
    interactive: bool,
    scope: Arc<Scope>,
}

#[derive(Debug, Default)]
struct Processes {
    running: Mutex<Vec<Process>>,
    finished: Condvar,
}

/// Cooperative cancellation. A child is cancelled when it or any ancestor is.
#[derive(Clone, Debug)]
pub struct Cancel {
    scope: Arc<Scope>,
    processes: Arc<Processes>,
}

/// Serializes process cleanup and reaping with cancellation's process-group kills.
pub(crate) struct CommandGuard<'a> {
    cancel: &'a Cancel,
    pid: u32,
    registered: bool,
}

impl CommandGuard<'_> {
    pub(crate) fn try_wait(&mut self, child: &mut Child) -> io::Result<Option<ExitStatus>> {
        let mut running = self
            .cancel
            .processes
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.registered
            && let Some(process) = running.iter().find(|process| process.pid == self.pid)
            && !process.interactive
        {
            // Observe exit without reaping: the root pid must still be reserved
            // while we kill descendants in its group, even after normal exit.
            // SAFETY: zero is valid for siginfo_t's scalar and pointer fields.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid writes within info; WNOWAIT preserves this child's
            // pid until group cleanup finishes and Child::try_wait reaps it.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    return Ok(None);
                }
                return Err(error);
            }
            // Darwin can report a stopped child despite WEXITED. Only terminal
            // exit events permit group cleanup; a stop must leave it alive.
            if !matches!(
                info.si_code,
                libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
            ) {
                return Ok(None);
            }
            terminate(self.pid, false);
        }
        let status = child.try_wait()?;
        if status.is_some() && self.registered {
            if let Some(index) = running.iter().position(|process| process.pid == self.pid) {
                running.swap_remove(index);
            }
            self.registered = false;
            self.cancel.processes.finished.notify_all();
        }
        Ok(status)
    }

    pub(crate) fn terminate(&self) {
        let running = self
            .cancel
            .processes
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.registered
            && let Some(process) = running.iter().find(|process| process.pid == self.pid)
        {
            terminate(process.pid, process.interactive);
        }
    }
}

impl Drop for CommandGuard<'_> {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        let mut running = self
            .cancel
            .processes
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = running.iter().position(|process| process.pid == self.pid) {
            let process = running.swap_remove(index);
            terminate(process.pid, process.interactive);
        }
        self.cancel.processes.finished.notify_all();
    }
}

pub(crate) fn terminate(pid: u32, interactive: bool) {
    let target = if interactive {
        pid as libc::pid_t
    } else {
        -(pid as libc::pid_t)
    };
    // SAFETY: kill takes scalar arguments. The registered child owns this pid
    // or the session/process group created for it; reaping uses the same lock.
    unsafe { libc::kill(target, libc::SIGKILL) };
}

impl Cancel {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::from_flag(Arc::new(AtomicBool::new(false)))
    }

    /// Wraps a flag owned elsewhere, such as one set by a signal handler.
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self {
            scope: Arc::new(Scope {
                cancelled: flag,
                parent: None,
            }),
            processes: Arc::new(Processes::default()),
        }
    }

    pub fn child(&self) -> Self {
        Self {
            scope: Arc::new(Scope {
                cancelled: Arc::new(AtomicBool::new(false)),
                parent: Some(self.scope.clone()),
            }),
            processes: self.processes.clone(),
        }
    }

    /// Cancels this token and its children, not its ancestors.
    pub fn cancel(&self) {
        self.scope.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        let mut scope = Some(self.scope.as_ref());
        while let Some(current) = scope {
            if current.cancelled.load(Ordering::SeqCst) {
                return true;
            }
            scope = current.parent.as_deref();
        }
        false
    }

    /// Cancels this scope, kills its registered processes/groups synchronously,
    /// then allows at most 400 ms for runners to reap them. Spawn and reaping use
    /// the same lock: late spawns are rejected and reaped pids cannot be killed.
    /// As with any syscall, OS scheduling may delay spawn or signal delivery.
    pub fn cancel_and_wait(&self) {
        let started = Instant::now();
        self.cancel();
        let running = self
            .processes
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for process in running
            .iter()
            .filter(|process| process.scope.descends_from(&self.scope))
        {
            terminate(process.pid, process.interactive);
        }
        drop(
            self.processes
                .finished
                .wait_timeout_while(
                    running,
                    Duration::from_millis(400).saturating_sub(started.elapsed()),
                    |running| {
                        running
                            .iter()
                            .any(|process| process.scope.descends_from(&self.scope))
                    },
                )
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    pub(crate) fn spawn_command(
        &self,
        command: &mut Command,
        interactive: bool,
    ) -> io::Result<Option<(Child, CommandGuard<'_>)>> {
        let mut running = self
            .processes
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_cancelled() {
            return Ok(None);
        }
        // Keep cancellation from overlooking a child between spawn and registration.
        let child = command.spawn()?;
        let pid = child.id();
        running.push(Process {
            pid,
            interactive,
            scope: self.scope.clone(),
        });
        Ok(Some((
            child,
            CommandGuard {
                cancel: self,
                pid,
                registered: true,
            },
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::Cancel;
    use crate::test_support::env_read;
    use std::os::unix::process::CommandExt;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn child_cancellation_does_not_reach_parent_or_siblings() {
        let root = Cancel::new();
        let first = root.child();
        let second = root.child();
        first.cancel();
        assert!(first.is_cancelled());
        assert!(!root.is_cancelled() && !second.is_cancelled());
        root.cancel();
        assert!(second.is_cancelled());
    }

    #[test]
    fn cancellation_prevents_a_spawn_waiting_for_registration() {
        let _environment = env_read();
        let root = Cancel::new();
        let runner_cancel = root.child();
        let shutdown_cancel = root.clone();
        let registration = root.processes.running.lock().unwrap();
        let runner = thread::spawn(move || {
            let mut command = Command::new("/bin/sleep");
            command.arg("30");
            assert!(
                runner_cancel
                    .spawn_command(&mut command, true)
                    .unwrap()
                    .is_none()
            );
        });
        let shutdown = thread::spawn(move || shutdown_cancel.cancel_and_wait());
        let deadline = Instant::now() + Duration::from_secs(1);
        while !root.is_cancelled() {
            assert!(Instant::now() < deadline, "shutdown did not begin");
            thread::yield_now();
        }
        drop(registration);
        runner.join().unwrap();
        shutdown.join().unwrap();
    }

    #[test]
    fn shutdown_returns_and_kills_before_the_runner_reaps() {
        let _environment = env_read();
        let cancel = Cancel::new();
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let (mut child, mut guard) = cancel.spawn_command(&mut command, true).unwrap().unwrap();
        // Keep the runner's registration alive until shutdown returns. A
        // scheduling delay is not a cancellation failure; waiting for this
        // runner to unregister would be.
        let shutdown_cancel = cancel.clone();
        let (finished, completion) = mpsc::channel();
        let shutdown = thread::spawn(move || {
            shutdown_cancel.cancel_and_wait();
            let _ = finished.send(());
        });
        completion
            .recv_timeout(Duration::from_secs(5))
            .expect("shutdown waited for the runner to reap");
        shutdown.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = guard.try_wait(&mut child).unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "child survived shutdown");
            thread::yield_now();
        };
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn stopped_background_child_is_not_mistaken_for_an_exit() {
        let _environment = env_read();
        let cancel = Cancel::new();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "kill -STOP $$; exit 7"])
            .process_group(0);
        let (mut child, mut guard) = cancel.spawn_command(&mut command, false).unwrap().unwrap();
        let pid = child.id() as libc::pid_t;
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // The child immediately stops itself. Observe without reaping it.
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WSTOPPED | libc::WEXITED | libc::WNOWAIT,
                )
            },
            0
        );
        assert_eq!(info.si_code, libc::CLD_STOPPED);
        assert!(guard.try_wait(&mut child).unwrap().is_none());
        assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = guard.try_wait(&mut child).unwrap() {
                assert_eq!(status.code(), Some(7));
                break;
            }
            assert!(Instant::now() < deadline, "continued child did not exit");
            thread::sleep(Duration::from_millis(5));
        }
    }
}
