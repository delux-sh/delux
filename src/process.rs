use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::cancel::{Cancel, CommandGuard};
use crate::terminal::{TerminalState, restore_terminal, terminal_state};

pub const COMMAND_OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    Timeout(Duration),
    Cancelled,
    OutputLimit(usize),
    InvalidUtf8,
    Io(String),
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(duration) => {
                if duration.subsec_nanos() == 0 {
                    write!(formatter, "Probe timed out after {}s", duration.as_secs())
                } else {
                    write!(
                        formatter,
                        "Probe timed out after {}ms",
                        duration.as_millis()
                    )
                }
            }
            Self::Cancelled => formatter.write_str("Probe cancelled"),
            Self::OutputLimit(limit) if limit.is_multiple_of(COMMAND_OUTPUT_LIMIT) => {
                write!(
                    formatter,
                    "Probe output exceeded {} MiB",
                    limit / COMMAND_OUTPUT_LIMIT
                )
            }
            Self::OutputLimit(limit) => write!(formatter, "Probe output exceeded {limit} bytes"),
            Self::InvalidUtf8 => formatter.write_str("Probe returned invalid UTF-8"),
            Self::Io(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for CommandError {}

struct RestoreStdin(Option<TerminalState>);

impl Drop for RestoreStdin {
    fn drop(&mut self) {
        if let Some(state) = &self.0 {
            restore_terminal(libc::STDIN_FILENO, state);
        }
    }
}

pub fn run_command(
    cancel: &Cancel,
    args: &[String],
    timeout: Duration,
    interactive: bool,
) -> Result<CommandResult, CommandError> {
    run_command_with_env(
        cancel,
        args,
        timeout,
        interactive,
        &[],
        COMMAND_OUTPUT_LIMIT,
    )
}

/// Captures a bounded combined output without blocking on descendants' inherited pipes.
/// Interactive children retain the controlling terminal; background probes run
/// in a new session so cancellation can terminate their whole process group.
pub fn run_command_with_env(
    cancel: &Cancel,
    args: &[String],
    timeout: Duration,
    interactive: bool,
    extra_env: &[(&str, &str)],
    output_limit: usize,
) -> Result<CommandResult, CommandError> {
    if cancel.is_cancelled() {
        return Err(CommandError::Cancelled);
    }
    if timeout.is_zero() {
        return Err(CommandError::Timeout(timeout));
    }
    let started = Instant::now();
    let _restore = RestoreStdin(if interactive {
        terminal_state(libc::STDIN_FILENO)
    } else {
        None
    });
    let mut command = Command::new(&args[0]);
    command.args(&args[1..]).stdout(Stdio::piped());
    if interactive {
        command.stdin(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        command.stdin(Stdio::null()).stderr(Stdio::piped());
        command
            .env_remove("SSH_ASKPASS")
            .env("SSH_ASKPASS_REQUIRE", "never");
        // SAFETY: this closure only calls the async-signal-safe setsid syscall
        // and constructs an errno value before exec, without allocation or locks.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    for &(name, value) in extra_env {
        command.env(name, value);
    }
    let (mut child, mut guard) = cancel
        .spawn_command(&mut command, interactive)
        .map_err(|error| CommandError::Io(format!("Starting probe: {error}")))?
        .ok_or(CommandError::Cancelled)?;
    let result = collect_output(
        &mut child,
        &mut guard,
        cancel,
        started,
        timeout,
        output_limit,
    );
    if result.is_err() {
        guard.terminate();
        // Never hold the registry lock while waiting for a killed child. Picker
        // shutdown can deliver SIGKILL and finish its bounded wait independently.
        while matches!(guard.try_wait(&mut child), Ok(None)) {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    result
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: both fcntl operations inspect/change flags on a borrowed live fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFL takes an integer flag word and preserves existing flags.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn collect_output(
    child: &mut Child,
    guard: &mut CommandGuard<'_>,
    cancel: &Cancel,
    started: Instant,
    timeout: Duration,
    output_limit: usize,
) -> Result<CommandResult, CommandError> {
    let mut pipes: [Option<OwnedFd>; 2] = [
        child.stdout.take().map(OwnedFd::from),
        child.stderr.take().map(OwnedFd::from),
    ];
    for pipe in pipes.iter().flatten() {
        nonblocking(pipe.as_raw_fd()).map_err(|error| CommandError::Io(error.to_string()))?;
    }
    let mut output = [Vec::new(), Vec::new()];
    let mut size = 0;
    let mut status: Option<ExitStatus> = None;
    let mut drain_started: Option<Instant> = None;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        if cancel.is_cancelled() {
            return Err(CommandError::Cancelled);
        }
        if started.elapsed() >= timeout {
            return Err(CommandError::Timeout(timeout));
        }
        if status.is_none() {
            status = guard
                .try_wait(child)
                .map_err(|error| CommandError::Io(format!("Waiting for probe: {error}")))?;
            if status.is_some() {
                drain_started = Some(Instant::now());
            }
        }
        if status.is_some()
            && (pipes.iter().all(Option::is_none)
                || drain_started.is_some_and(|start| start.elapsed() >= Duration::from_millis(100)))
        {
            break;
        }
        let mut poll = pipes.each_ref().map(|pipe| libc::pollfd {
            fd: pipe.as_ref().map_or(-1, AsRawFd::as_raw_fd),
            events: libc::POLLIN,
            revents: 0,
        });
        // Short polling also detects root exit when descendants keep its pipes
        // open. Each iteration reads at most one chunk per stream for fairness.
        let mut delay = Duration::from_millis(10).min(timeout.saturating_sub(started.elapsed()));
        if let Some(start) = drain_started {
            delay = delay.min(Duration::from_millis(100).saturating_sub(start.elapsed()));
        }
        let millis = delay.as_millis().max(1) as libc::c_int;
        // SAFETY: poll is a valid array of two descriptors; -1 entries are ignored.
        let ready = unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as libc::nfds_t, millis) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(CommandError::Io(error.to_string()));
        }
        for index in 0..pipes.len() {
            if poll[index].revents == 0 {
                continue;
            }
            if poll[index].revents & libc::POLLNVAL != 0 {
                return Err(CommandError::Io(
                    io::Error::from_raw_os_error(libc::EBADF).to_string(),
                ));
            }
            // SAFETY: buffer is writable for its length and the descriptor is
            // owned by pipes[index], which remains live for the read.
            let count =
                unsafe { libc::read(poll[index].fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count < 0 {
                let error = io::Error::last_os_error();
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) {
                    continue;
                }
                return Err(CommandError::Io(error.to_string()));
            }
            if count == 0 {
                pipes[index] = None;
                continue;
            }
            let count = count as usize;
            if count > output_limit - size {
                return Err(CommandError::OutputLimit(output_limit));
            }
            size += count;
            output[index].extend_from_slice(&buffer[..count]);
        }
    }
    let [stdout, stderr] = output;
    Ok(CommandResult {
        code: status.and_then(|status| status.code()).unwrap_or(-1),
        stdout: String::from_utf8(stdout).map_err(|_| CommandError::InvalidUtf8)?,
        stderr: String::from_utf8(stderr)
            .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()),
    })
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::path::Path;
    use std::sync::mpsc;
    use std::thread;

    use super::*;
    use crate::cancel::terminate;
    use crate::test_support::{EnvVar, TempDir, env_read, env_write, open_pty};

    fn shell(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    fn shell_with_path(script: &str, path: &Path) -> Vec<String> {
        let mut arguments = shell(script);
        arguments.push("delux-fixture".into());
        arguments.push(path.to_str().unwrap().into());
        arguments
    }

    #[test]
    fn run_command_separates_streams_and_exit_code() {
        let _environment = env_read();
        let result = run_command(
            &Cancel::new(),
            &shell("printf 'inventory 東京\\n'; printf 'warning\\n' >&2; exit 7"),
            Duration::from_secs(5),
            false,
        )
        .unwrap();
        assert_eq!(
            result,
            CommandResult {
                code: 7,
                stdout: "inventory 東京\n".into(),
                stderr: "warning\n".into(),
            }
        );
    }

    #[test]
    fn run_command_validates_utf8() {
        let _environment = env_read();
        assert_eq!(
            run_command(
                &Cancel::new(),
                &shell("printf 'x\\377y'"),
                Duration::from_secs(5),
                false
            ),
            Err(CommandError::InvalidUtf8),
        );
        let result = run_command(
            &Cancel::new(),
            &shell("printf 'x\\377y' >&2"),
            Duration::from_secs(5),
            false,
        )
        .unwrap();
        assert_eq!(result.stderr, "x\u{fffd}y");
        let result = run_command(
            &Cancel::new(),
            &shell("printf 'x\\377\\376y' >&2"),
            Duration::from_secs(5),
            false,
        )
        .unwrap();
        assert_eq!(result.stderr, "x\u{fffd}\u{fffd}y");
    }

    #[test]
    fn run_command_enforces_combined_streaming_budget() {
        let _environment = env_read();
        let started = Instant::now();
        let result = run_command(
            &Cancel::new(),
            &shell(
                "/bin/dd if=/dev/zero bs=1024 count=600 2>/dev/null; \
                 /bin/dd if=/dev/zero bs=1024 count=600 >&2 2>/dev/null; exec /bin/sleep 30",
            ),
            Duration::from_secs(5),
            false,
        );
        assert_eq!(result, Err(CommandError::OutputLimit(COMMAND_OUTPUT_LIMIT)));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "overflow waited for the deadline"
        );
        let result = run_command(
            &Cancel::new(),
            &shell(
                "/bin/dd if=/dev/zero bs=1024 count=512 2>/dev/null; \
                 /bin/dd if=/dev/zero bs=1024 count=512 >&2 2>/dev/null",
            ),
            Duration::from_secs(5),
            false,
        )
        .unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.stdout.len(), COMMAND_OUTPUT_LIMIT / 2);
        assert_eq!(result.stderr.len(), COMMAND_OUTPUT_LIMIT / 2);
    }

    #[test]
    fn per_command_output_limit_can_exceed_the_probe_default() {
        let _environment = env_read();
        let cancel = Cancel::new();
        let arguments = shell("/bin/dd if=/dev/zero bs=1024 count=1536 2>/dev/null");
        assert_eq!(
            run_command(&cancel, &arguments, Duration::from_secs(5), false),
            Err(CommandError::OutputLimit(COMMAND_OUTPUT_LIMIT)),
        );
        let larger_limit = 2 * COMMAND_OUTPUT_LIMIT;
        let result = run_command_with_env(
            &cancel,
            &arguments,
            Duration::from_secs(5),
            false,
            &[],
            larger_limit,
        )
        .unwrap();
        assert_eq!(result.stdout.len(), 1536 * 1024);
        let overflow = shell("/bin/dd if=/dev/zero bs=1024 count=2049 2>/dev/null");
        let error = run_command_with_env(
            &cancel,
            &overflow,
            Duration::from_secs(5),
            false,
            &[],
            larger_limit,
        )
        .unwrap_err();
        assert_eq!(error, CommandError::OutputLimit(larger_limit));
        assert_eq!(error.to_string(), "Probe output exceeded 2 MiB");
    }

    #[test]
    fn run_command_deadline_and_pre_cancellation() {
        let _environment = env_read();
        let started = Instant::now();
        let timeout = Duration::from_millis(100);
        assert_eq!(
            run_command(&Cancel::new(), &shell("exec /bin/sleep 30"), timeout, false),
            Err(CommandError::Timeout(timeout)),
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        let directory = TempDir::new();
        let marker = directory.path().join("started");
        let cancel = Cancel::new();
        cancel.cancel();
        let arguments = shell_with_path("printf started > \"$1\"", &marker);
        assert_eq!(
            run_command(&cancel, &arguments, Duration::from_secs(5), false),
            Err(CommandError::Cancelled),
        );
        assert!(!marker.exists(), "pre-cancelled probe started a child");
        assert_eq!(
            run_command(&Cancel::new(), &arguments, Duration::ZERO, false),
            Err(CommandError::Timeout(Duration::ZERO)),
        );
        assert!(!marker.exists(), "expired probe started a child");
    }

    #[test]
    fn run_command_noninteractive_environment_and_stdin() {
        let _environment = env_write();
        let _askpass = EnvVar::set("SSH_ASKPASS", "must-not-run");
        let _require = EnvVar::set("SSH_ASKPASS_REQUIRE", "force");
        let result = run_command(
            &Cancel::new(),
            &shell(
                r#"[ "${SSH_ASKPASS+x}" != x ] && [ "$SSH_ASKPASS_REQUIRE" = never ] || exit 93
                if IFS= read -r input; then exit 94; fi
                printf 'stdin EOF\n'"#,
            ),
            Duration::from_secs(5),
            false,
        )
        .unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.stdout, "stdin EOF\n");
        let _tailscale = EnvVar::set("TAILSCALE_BE_CLI", "old");
        let result = run_command_with_env(
            &Cancel::new(),
            &shell("printf %s \"$TAILSCALE_BE_CLI\""),
            Duration::from_secs(5),
            false,
            &[("TAILSCALE_BE_CLI", "1")],
            COMMAND_OUTPUT_LIMIT,
        )
        .unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.stdout, "1");
        assert_eq!(std::env::var("TAILSCALE_BE_CLI").unwrap(), "old");
    }

    struct Descendant(libc::pid_t);

    impl Drop for Descendant {
        fn drop(&mut self) {
            // SAFETY: this is a best-effort cleanup of the fixture's recorded pid.
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }

    fn descendant_arguments(path: &Path, exit_root: bool) -> Vec<String> {
        let mut script = String::from(
            r#"/bin/sh -c 'printf %s "$$" > "$1"; exec /bin/sleep 30' descendant "$1" &
            while [ ! -s "$1" ]; do /bin/sleep 0.01; done
            "#,
        );
        if exit_root {
            script.push_str("printf 'root finished\\n'");
        } else {
            script.push_str("exec /bin/sleep 30");
        }
        shell_with_path(&script, path)
    }

    fn await_descendant_pid(path: &Path) -> Descendant {
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if let Ok(data) = std::fs::read_to_string(path)
                && let Ok(pid) = data.parse::<libc::pid_t>()
                && pid > 0
            {
                return Descendant(pid);
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("descendant did not report readiness");
    }

    fn await_descendant_exit(descendant: &Descendant) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            // SAFETY: signal zero checks existence without delivering a signal.
            if unsafe { libc::kill(descendant.0, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            #[cfg(target_os = "linux")]
            {
                // Container PID 1 can leave a killed, orphaned child as a zombie.
                match std::fs::read(format!("/proc/{}/stat", descendant.0)) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return,
                    Ok(data) => {
                        if let Some(closing) = data.iter().rposition(|&byte| byte == b')')
                            && data.get(closing + 2) == Some(&b'Z')
                        {
                            return;
                        }
                    }
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("descendant {} survived probe cleanup", descendant.0);
    }

    #[test]
    fn cancellation_wait_kills_descendant_process_groups_within_half_a_second() {
        let _environment = env_read();
        let directory = TempDir::new();
        let pid_file = directory.path().join("descendant");
        let arguments = descendant_arguments(&pid_file, false);
        let root = Cancel::new();
        let cancel = root.child();
        let runner_cancel = cancel.child();
        let (finished, receive) = mpsc::channel();
        let runner = thread::spawn(move || {
            finished
                .send(run_command(
                    &runner_cancel,
                    &arguments,
                    Duration::from_secs(10),
                    false,
                ))
                .unwrap();
        });
        let descendant = await_descendant_pid(&pid_file);
        let started = Instant::now();
        cancel.cancel_and_wait();
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(!root.is_cancelled());
        assert_eq!(
            receive
                .recv_timeout(Duration::from_secs(3))
                .expect("cancelled process waited on inherited pipes"),
            Err(CommandError::Cancelled),
        );
        runner.join().unwrap();
        await_descendant_exit(&descendant);
    }

    #[test]
    fn run_command_deadline_kills_descendant_process_group() {
        let _environment = env_read();
        let directory = TempDir::new();
        let pid_file = directory.path().join("descendant");
        let timeout = Duration::from_secs(2);
        assert_eq!(
            run_command(
                &Cancel::new(),
                &descendant_arguments(&pid_file, false),
                timeout,
                false
            ),
            Err(CommandError::Timeout(timeout)),
        );
        let descendant = await_descendant_pid(&pid_file);
        await_descendant_exit(&descendant);
    }

    #[test]
    fn run_command_root_exit_cannot_leave_inherited_pipes_open() {
        let _environment = env_read();
        let directory = TempDir::new();
        let pid_file = directory.path().join("descendant");
        let started = Instant::now();
        let result = run_command(
            &Cancel::new(),
            &descendant_arguments(&pid_file, true),
            Duration::from_secs(5),
            false,
        )
        .unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.stdout, "root finished\n");
        assert!(started.elapsed() < Duration::from_secs(3));
        let descendant = await_descendant_pid(&pid_file);
        await_descendant_exit(&descendant);
    }

    #[test]
    fn run_command_errors_do_not_expose_arguments() {
        let _environment = env_read();
        let error = run_command(
            &Cancel::new(),
            &["/no-such-delux-probe".into(), "secret-argument".into()],
            Duration::from_secs(1),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().starts_with("Starting probe: "));
        assert!(!error.to_string().contains("secret-argument"));
        assert_eq!(
            CommandError::Timeout(Duration::from_secs(12)).to_string(),
            "Probe timed out after 12s"
        );
        assert_eq!(
            CommandError::Timeout(Duration::from_millis(100)).to_string(),
            "Probe timed out after 100ms"
        );
    }

    fn terminal_settings(fd: RawFd) -> libc::termios {
        // SAFETY: termios contains only integers, for which zero is valid.
        let mut state: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: initialized storage covers the full struct, including c_cc
        // entries that tcgetattr may leave untouched on platforms such as musl.
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut state) }, 0);
        // BSD sets PENDIN on return to canonical mode; it is transient state.
        state.c_lflag &= !libc::PENDIN;
        state
    }

    fn assert_terminal_settings_equal(before: &libc::termios, after: &libc::termios) {
        assert_eq!(before.c_iflag, after.c_iflag);
        assert_eq!(before.c_oflag, after.c_oflag);
        assert_eq!(before.c_cflag, after.c_cflag);
        assert_eq!(before.c_lflag, after.c_lflag);
        assert_eq!(before.c_cc, after.c_cc);
        // SAFETY: these functions only read initialized termios values.
        unsafe {
            assert_eq!(libc::cfgetispeed(before), libc::cfgetispeed(after));
            assert_eq!(libc::cfgetospeed(before), libc::cfgetospeed(after));
        }
        #[cfg(target_os = "linux")]
        assert_eq!(before.c_line, after.c_line);
    }

    // Re-executed inside a session whose stdin and stderr are the PTY slave.
    // The normal test run has no mode and returns without creating a subprocess.
    #[test]
    fn command_helper_process() {
        let _environment = env_read();
        let Some(mode) = std::env::var_os("DELUX_PROCESS_TEST_MODE") else {
            return;
        };
        match mode.to_str().unwrap() {
            "interactive-runner" => {
                let result = run_command(
                    &Cancel::new(),
                    &shell(
                        r#"[ -t 0 ] && [ -t 2 ] || exit 99
                        [ "$SSH_ASKPASS" = original-askpass ] && [ "$SSH_ASKPASS_REQUIRE" = prefer ] || exit 99
                        exec 3<> /dev/tty || exit 100
                        exec 3>&-
                        printf 'authentication prompt\n' >&2
                        IFS= read -r input || exit 101
                        [ "$input" = answer ] || exit 101
                        printf 'interactive inventory\n'"#,
                    ),
                    Duration::from_secs(3),
                    true,
                ).unwrap();
                assert_eq!(result.code, 0);
                assert_eq!(result.stdout, "interactive inventory\n");
                assert_eq!(result.stderr, "");
                print!("{}", result.stdout);
            }
            "interactive-cancel-runner" => {
                let marker = std::env::var_os("DELUX_PROCESS_TEST_MARKER").unwrap();
                let before = terminal_settings(libc::STDIN_FILENO);
                let timeout = Duration::from_secs(1);
                let result = run_command(
                    &Cancel::new(),
                    &shell_with_path(
                        "stty raw || exit 105; printf raw > \"$1\" || exit 106; \
                         printf 'authentication prompt\\n' >&2; exec /bin/sleep 30",
                        Path::new(&marker),
                    ),
                    timeout,
                    true,
                );
                let after = terminal_settings(libc::STDIN_FILENO);
                assert_eq!(result, Err(CommandError::Timeout(timeout)));
                assert_terminal_settings_equal(&before, &after);
                assert_eq!(std::fs::read(&marker).unwrap(), b"raw");
                println!("terminal restored");
            }
            mode => panic!("unknown process helper mode: {mode}"),
        }
        io::stdout().flush().unwrap();
        // Do not append the test harness's summary to the runner's output.
        std::process::exit(0);
    }

    #[test]
    fn run_command_interactive_retains_controlling_terminal() {
        run_interactive_command_helper("interactive-runner", "interactive inventory\n");
    }

    #[test]
    fn run_command_interactive_cancellation_restores_terminal() {
        run_interactive_command_helper("interactive-cancel-runner", "terminal restored\n");
    }

    fn run_interactive_command_helper(mode: &str, expected: &str) {
        let _environment = env_write();
        let _askpass = EnvVar::set("SSH_ASKPASS", "original-askpass");
        let _require = EnvVar::set("SSH_ASKPASS_REQUIRE", "prefer");
        let directory = TempDir::new();
        let (master, slave) = open_pty(24, 80);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "process::tests::command_helper_process",
            "--nocapture",
            "--test-threads=1",
        ]);
        command
            .env("DELUX_PROCESS_TEST_MODE", mode)
            .env(
                "DELUX_PROCESS_TEST_MARKER",
                directory.path().join("raw-ready"),
            )
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::piped());
        // SAFETY: only async-signal-safe session/terminal syscalls run between
        // fork and exec. fd 0 has already been redirected to the PTY slave.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn().unwrap();
        drop(command);
        drop(slave);
        let pid = child.id();
        let mut master = File::from(master);
        let mut terminal_reader = master.try_clone().unwrap();
        let (terminal_output, receive_terminal) = mpsc::channel();
        // A macOS session leader can wait for unread terminal output on exit.
        // Read concurrently until all slave fds close: EOF on macOS, EIO on Linux.
        let reader = thread::spawn(move || {
            let mut output = Vec::new();
            let result = terminal_reader.read_to_end(&mut output);
            if let Err(error) = result {
                assert_eq!(error.raw_os_error(), Some(libc::EIO), "{error}");
            }
            terminal_output.send(output).unwrap();
        });
        if let Err(error) = master.write_all(b"answer\n") {
            terminate(pid, false);
            let _ = child.wait();
            panic!("writing terminal input: {error}");
        }
        drop(master);
        let mut stdout = child.stdout.take().unwrap();
        let (stdout_output, receive_stdout) = mpsc::channel();
        let stdout_reader = thread::spawn(move || {
            let mut output = String::new();
            stdout.read_to_string(&mut output).unwrap();
            stdout_output.send(output).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(6);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                terminate(pid, false);
                let _ = child.wait();
                panic!("interactive runner exceeded its deadline");
            }
            thread::sleep(Duration::from_millis(10));
        };
        terminate(pid, false);
        let stdout = receive_stdout.recv_timeout(Duration::from_secs(2)).unwrap();
        let terminal = receive_terminal
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        stdout_reader.join().unwrap();
        reader.join().unwrap();
        assert!(
            status.success(),
            "runner status: {status}; stdout: {stdout:?}; terminal: {:?}",
            String::from_utf8_lossy(&terminal)
        );
        // libtest prints a test-name prefix before entering the helper; the
        // helper itself asserts the exact captured stdout and empty stderr.
        assert!(stdout.ends_with(expected), "interactive stdout: {stdout:?}");
        assert!(
            String::from_utf8_lossy(&terminal).contains("authentication prompt"),
            "stderr did not reach the terminal: {:?}",
            String::from_utf8_lossy(&terminal)
        );
    }
}
