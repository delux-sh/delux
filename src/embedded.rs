//! A private PTY for one SSH/tmux client. Nothing received from the child is
//! written directly to Delux's terminal: only parsed cells reach the renderer.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::io::{self, Read};
use std::num::NonZeroU16;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStdout, Command, Stdio};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

const INPUT_LIMIT: usize = 1024 * 1024;
const IO_BUDGET: usize = 64 * 1024;
const IO_CALLS: usize = 32;

/// Small escape sequences are formatted on the stack, including query replies.
struct Sequence {
    bytes: [u8; 64],
    len: usize,
}

impl Sequence {
    fn new() -> Self {
        Self {
            bytes: [0; 64],
            len: 0,
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl fmt::Write for Sequence {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len + text.len();
        let dest = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        dest.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[derive(Default)]
struct Replies {
    // Input and replies share one FIFO, preserving ordering and a single bound.
    pending: VecDeque<u8>,
    overflow: bool,
}

impl Replies {
    fn enqueue(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > INPUT_LIMIT.saturating_sub(self.pending.len()) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "terminal input queue is full (1 MiB maximum)",
            ));
        }
        self.pending.extend(bytes);
        Ok(())
    }

    fn reply(&mut self, bytes: &[u8]) {
        if self.enqueue(bytes).is_err() {
            self.overflow = true;
        }
    }
}

impl vt100::Callbacks for Replies {
    fn unhandled_escape(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        byte: u8,
    ) {
        if i1.is_none() && i2.is_none() && byte == b'Z' {
            self.reply(b"\x1b[?1;2c");
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        if i2.is_some() || params.len() > 1 || params.iter().any(|p| p.len() > 1) {
            return;
        }
        let param = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let mut reply = Sequence::new();
        match (i1, command, param) {
            (None, 'n', 5) => self.reply(b"\x1b[0n"),
            (None | Some(b'?'), 'n', 6) => {
                let (row, col) = screen.cursor_position();
                let private = if i1.is_some() { "?" } else { "" };
                let _ = write!(
                    reply,
                    "\x1b[{private}{};{}R",
                    u32::from(row) + 1,
                    u32::from(col) + 1
                );
                self.reply(reply.bytes());
            }
            (None, 'c', 0) => self.reply(b"\x1b[?1;2c"),
            (Some(b'>'), 'c', 0) => self.reply(b"\x1b[>0;0;0c"),
            (None, 't', 18 | 19) => {
                let (rows, cols) = screen.size();
                let report = if param == 18 { 8 } else { 9 };
                let _ = write!(reply, "\x1b[{report};{rows};{cols}t");
                self.reply(reply.bytes());
            }
            _ => {}
        }
    }
    // The default callbacks deliberately ignore titles, clipboard operations,
    // bells and child-requested window resizing. The picker owns its geometry.
}

struct CapturedOutput {
    stream: ChildStdout,
    bytes: Vec<u8>,
}

pub(crate) struct EmbeddedTerminal {
    master: Option<OwnedFd>,
    child: Option<Child>,
    parser: vt100::Parser<Replies>,
    exit_status: Option<i32>,
    eof: bool,
    output: Option<CapturedOutput>,
}

impl EmbeddedTerminal {
    pub(crate) fn spawn(args: &[String], rows: u16, cols: u16) -> io::Result<Self> {
        Self::spawn_inner(args, rows, cols, false)
    }

    /// Keep machine-readable stdout separate from prompts on the controlling PTY.
    pub(crate) fn spawn_captured(args: &[String], rows: u16, cols: u16) -> io::Result<Self> {
        Self::spawn_inner(args, rows, cols, true)
    }

    fn spawn_inner(args: &[String], rows: u16, cols: u16, capture: bool) -> io::Result<Self> {
        let program = args
            .first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty terminal command"))?;
        let (rows, cols) = dimensions(rows, cols);
        let (master, slave) = open_pty(rows.get(), cols.get())?;
        let mut command = Command::new(program);
        command
            .args(&args[1..])
            .env("TERM", "xterm-256color")
            .env_remove("LINES")
            .env_remove("COLUMNS")
            // This PTY is a new client, even when Delux runs inside tmux.
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("SSH_ASKPASS")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(if capture {
                Stdio::piped()
            } else {
                Stdio::from(slave.try_clone()?)
            })
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe syscalls and errno conversion run
        // between fork and exec. Stdio has already been installed on fd 0.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1
                    || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let output = child.stdout.take().map(|stream| CapturedOutput {
            stream,
            bytes: Vec::new(),
        });
        let terminal = Self {
            master: Some(master),
            child: Some(child),
            parser: vt100::Parser::new_with_callbacks(rows, cols, 0, Replies::default()),
            exit_status: None,
            eof: false,
            output,
        };
        if let Some(output) = &terminal.output {
            // `terminal` owns the child already, so failure still tears it down.
            nonblocking(output.stream.as_raw_fd())?;
        }
        Ok(terminal)
    }

    pub(crate) fn captured_output(&self) -> &[u8] {
        self.output.as_ref().map_or(&[], |output| &output.bytes)
    }

    pub(crate) fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    fn read_captured_output(&mut self) -> io::Result<()> {
        let Some(output) = &mut self.output else {
            return Ok(());
        };
        let mut buffer = [0; 4096];
        let mut total = 0;
        for _ in 0..IO_CALLS {
            match output.stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if output.bytes.len() + count > crate::process::COMMAND_OUTPUT_LIMIT {
                        return Err(io::Error::other("Probe output exceeded 1 MiB"));
                    }
                    output.bytes.extend_from_slice(&buffer[..count]);
                    total += count;
                    if total >= IO_BUDGET {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Bounded work even when the child continuously produces output. Observing
    /// exit with WNOWAIT reserves the PID until process-group cleanup is done.
    pub(crate) fn poll(&mut self) -> io::Result<Option<i32>> {
        let already_exited = self.exit_status.is_some();
        self.flush_input()?;
        self.read_captured_output()?;
        self.read_output()?;
        if self.exit_status.is_none() {
            // SAFETY: zero is valid for siginfo_t; waitid writes to this local.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: this Child exclusively owns its unreaped PID. WNOWAIT
            // prevents PID reuse between observation and group cleanup.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.as_ref().expect("owned terminal child").id() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == -1 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            } else if matches!(
                info.si_code,
                libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
            ) {
                // Darwin may report CLD_STOPPED even with WEXITED; it is not
                // permission to kill or reap a still-live client.
                self.kill_group();
                if let Some(status) = self
                    .child
                    .as_mut()
                    .expect("owned terminal child")
                    .try_wait()?
                {
                    self.exit_status = Some(
                        status
                            .code()
                            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
                    );
                    self.parser.callbacks_mut().pending.clear();
                }
            }
        }
        self.flush_input()?;
        // One final bounded drain after exit captures the remaining PTY buffer,
        // without following a daemonized descendant that keeps writing forever.
        Ok(if already_exited {
            self.exit_status
        } else {
            None
        })
    }

    fn read_output(&mut self) -> io::Result<()> {
        if self.eof {
            return Ok(());
        }
        let mut buffer = [0; 4096];
        let mut total = 0;
        for _ in 0..IO_CALLS {
            // SAFETY: master is live and the buffer is writable for its length.
            let count =
                unsafe { libc::read(self.master_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if count > 0 {
                let count = count as usize;
                self.parser.process(&buffer[..count]);
                if self.parser.callbacks().overflow {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "terminal query replies exceeded the input queue",
                    ));
                }
                total += count;
                if total >= IO_BUDGET {
                    return Ok(());
                }
            } else if count == 0 {
                self.eof = true;
                return Ok(());
            } else {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EIO) {
                    // Linux reports a closed PTY slave as EIO instead of EOF.
                    self.eof = true;
                    return Ok(());
                }
                match error.kind() {
                    io::ErrorKind::WouldBlock => return Ok(()),
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(error),
                }
            }
        }
        Ok(())
    }

    fn flush_input(&mut self) -> io::Result<()> {
        if self.eof || self.exit_status.is_some() {
            return Ok(());
        }
        let master = self.master_fd();
        let queue = &mut self.parser.callbacks_mut().pending;
        let mut total = 0;
        for _ in 0..IO_CALLS {
            let (first, second) = queue.as_slices();
            let bytes = if first.is_empty() { second } else { first };
            let bytes = &bytes[..bytes.len().min(IO_BUDGET - total)];
            if bytes.is_empty() {
                break;
            }
            // SAFETY: bytes refers to initialized storage, and master is live.
            let count = unsafe { libc::write(master, bytes.as_ptr().cast(), bytes.len()) };
            if count > 0 {
                let count = count as usize;
                queue.drain(..count);
                total += count;
                if total == IO_BUDGET {
                    break;
                }
            } else if count == 0 {
                break;
            } else {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EIO)
                    || error.kind() == io::ErrorKind::BrokenPipe
                {
                    queue.clear();
                    break;
                }
                match error.kind() {
                    io::ErrorKind::WouldBlock => break,
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(error),
                }
            }
        }
        Ok(())
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) -> io::Result<()> {
        let (rows, cols) = dimensions(rows, cols);
        if self.parser.screen().size() == (rows, cols) {
            return Ok(());
        }
        let size = libc::winsize {
            ws_row: rows.get(),
            ws_col: cols.get(),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: size points to a valid winsize and master is a live PTY.
        if unsafe { libc::ioctl(self.master_fd(), libc::TIOCSWINSZ, &size) } == -1 {
            return Err(io::Error::last_os_error());
        }
        self.parser.set_size(rows, cols);
        Ok(())
    }

    pub(crate) fn send_key(&mut self, key: KeyEvent) -> io::Result<()> {
        if key.kind == KeyEventKind::Release {
            return Ok(());
        }
        let sequence = encode_key(key, self.parser.screen().application_cursor());
        self.parser.callbacks_mut().enqueue(sequence.bytes())?;
        self.flush_input()
    }

    /// Reject a paste atomically if it will not fit: never queue a partial
    /// bracketed-paste frame, and never block the picker waiting for SSH.
    pub(crate) fn paste(&mut self, text: &str) -> io::Result<()> {
        let bracketed = self.parser.screen().bracketed_paste();
        let queue = &mut self.parser.callbacks_mut().pending;
        let overhead = if bracketed { 12 } else { 0 };
        if text.len().saturating_add(overhead) > INPUT_LIMIT.saturating_sub(queue.len()) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "paste exceeds available terminal input queue (1 MiB maximum)",
            ));
        }
        if bracketed {
            queue.extend(b"\x1b[200~");
            queue.extend(text.as_bytes());
            queue.extend(b"\x1b[201~");
        } else {
            // A literal paste newline is an Enter key on the PTY. Preserve CRLF
            // as one Enter rather than accidentally submitting an empty line.
            let mut previous = 0;
            for byte in text.bytes() {
                if byte != b'\n' || previous != b'\r' {
                    queue.push_back(if byte == b'\n' { b'\r' } else { byte });
                }
                previous = byte;
            }
        }
        self.flush_input()
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, focused: bool) {
        let area = area.intersection(frame.area());
        let screen = self.parser.screen();
        let buffer = frame.buffer_mut();
        for row in 0..area.height {
            for col in 0..area.width {
                let target = &mut buffer[(area.x + col, area.y + row)];
                target.reset();
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                let mut modifiers = Modifier::empty();
                for (enabled, modifier) in [
                    (cell.bold(), Modifier::BOLD),
                    (cell.dim(), Modifier::DIM),
                    (cell.italic(), Modifier::ITALIC),
                    (cell.underline(), Modifier::UNDERLINED),
                    (cell.inverse(), Modifier::REVERSED),
                ] {
                    if enabled {
                        modifiers.insert(modifier);
                    }
                }
                target.set_style(
                    Style::default()
                        .fg(color(cell.fgcolor()))
                        .bg(color(cell.bgcolor()))
                        .add_modifier(modifiers),
                );
                // Ratatui's diff understands the width of the leading cell.
                // Reset continuation cells, including stale wide characters.
                if cell.has_contents()
                    && !cell.is_wide_continuation()
                    && (!cell.is_wide() || col + 1 < area.width)
                {
                    target.set_symbol(cell.contents());
                }
            }
        }
        let (row, col) = screen.cursor_position();
        if focused && !screen.hide_cursor() && row < area.height && col < area.width {
            frame.set_cursor_position((area.x + col, area.y + row));
        }
    }

    fn master_fd(&self) -> RawFd {
        self.master.as_ref().expect("open terminal PTY").as_raw_fd()
    }

    fn kill_group(&self) {
        // SAFETY: only called before this Child has been reaped, so its PID
        // cannot be reused. setsid made that PID the client's process group.
        // A ControlPersist master daemonizes into a different session/group;
        // tmux's server likewise remains independent of this attached client.
        let pid = self.child.as_ref().expect("owned terminal child").id();
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    }
}

impl Drop for EmbeddedTerminal {
    fn drop(&mut self) {
        // Release the PTY before reaping: macOS can hold an exiting client in
        // tty teardown while its master remains open with unread output.
        drop(self.master.take());
        if self.exit_status.is_none() {
            self.kill_group();
            // A session leader cannot change its process group, so the group
            // kill above always reaches the unreaped root.
            if let Some(mut child) = self.child.take()
                && !matches!(child.try_wait(), Ok(Some(_)))
            {
                // A slow kernel teardown must never block focus changes,
                // replacement, or Ctrl-C. Transfer exclusive reap ownership.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
        }
    }
}

fn dimensions(rows: u16, cols: u16) -> (NonZeroU16, NonZeroU16) {
    (
        NonZeroU16::new(rows.max(1)).unwrap(),
        NonZeroU16::new(cols.max(1)).unwrap(),
    )
}

#[allow(clippy::unnecessary_mut_passed)] // macOS declares openpty pointers mutable.
fn open_pty(rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: all output pointers are valid. Null termios requests the new
    // slave's cooked platform defaults, NOT the picker's raw-mode stdin flags.
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty transferred ownership of both fresh file descriptors.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        // SAFETY: fcntl modifies only flags on this owned file descriptor.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    nonblocking(master.as_raw_fd())?;
    Ok((master, slave))
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: the caller owns this live descriptor; preserve its existing flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn encode_key(mut key: KeyEvent, application_cursor: bool) -> Sequence {
    let mut output = Sequence::new();
    // xterm-256color's F13..F63 are modifier banks of F1..F12.
    if let KeyCode::F(number @ 13..=63) = key.code {
        let implied = match (number - 1) / 12 {
            1 => KeyModifiers::SHIFT,
            2 => KeyModifiers::CONTROL,
            3 => KeyModifiers::SHIFT | KeyModifiers::CONTROL,
            4 => KeyModifiers::ALT,
            _ => KeyModifiers::SHIFT | KeyModifiers::ALT,
        };
        key.modifiers.insert(implied);
        key.code = KeyCode::F((number - 1) % 12 + 1);
    }
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(control);
    let final_byte = match key.code {
        KeyCode::Up => Some('A'),
        KeyCode::Down => Some('B'),
        KeyCode::Right => Some('C'),
        KeyCode::Left => Some('D'),
        KeyCode::Home => Some('H'),
        KeyCode::End => Some('F'),
        KeyCode::KeypadBegin => Some('E'),
        KeyCode::F(1..=4) => Some(char::from(
            b'P' + match key.code {
                KeyCode::F(n) => n - 1,
                _ => unreachable!(),
            },
        )),
        _ => None,
    };
    if let Some(final_byte) = final_byte {
        if modifier != 1 {
            let _ = write!(output, "\x1b[1;{modifier}{final_byte}");
        } else if application_cursor || matches!(key.code, KeyCode::F(1..=4)) {
            let _ = write!(output, "\x1bO{final_byte}");
        } else {
            let _ = write!(output, "\x1b[{final_byte}");
        }
        return output;
    }
    let tilde = match key.code {
        KeyCode::Insert => Some(2),
        KeyCode::Delete => Some(3),
        KeyCode::PageUp => Some(5),
        KeyCode::PageDown => Some(6),
        KeyCode::F(5..=12) => {
            const CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            match key.code {
                KeyCode::F(n) => Some(CODES[usize::from(n - 5)]),
                _ => unreachable!(),
            }
        }
        _ => None,
    };
    if let Some(number) = tilde {
        if modifier == 1 {
            let _ = write!(output, "\x1b[{number}~");
        } else {
            let _ = write!(output, "\x1b[{number};{modifier}~");
        }
        return output;
    }
    // Modifier-only and multimedia events have no legacy terminal encoding.
    // Shift-Tab is consumed by the UI, but remains correctly encoded here.
    let character = match key.code {
        KeyCode::Char(c) if control => match c {
            ' ' | '@' | '2' => Some('\0'),
            'a'..='z' | 'A'..='Z' | '['..='_' => char::from_u32((c as u32) & 0x1f),
            '3' => Some('\x1b'),
            '4' => Some('\x1c'),
            '5' => Some('\x1d'),
            '6' => Some('\x1e'),
            '7' | '/' => Some('\x1f'),
            '8' | '?' => Some('\x7f'),
            _ => Some(c),
        },
        KeyCode::Char(c) => Some(c),
        KeyCode::Enter => Some('\r'),
        KeyCode::Tab if !shift => Some('\t'),
        KeyCode::Backspace => Some(if control { '\x08' } else { '\x7f' }),
        KeyCode::Esc => Some('\x1b'),
        KeyCode::Null => Some('\0'),
        _ => None,
    };
    if let Some(character) = character {
        if alt {
            let _ = output.write_char('\x1b');
        }
        let _ = output.write_char(character);
    } else if key.code == KeyCode::BackTab || (key.code == KeyCode::Tab && shift) {
        if alt {
            let _ = output.write_char('\x1b');
        }
        let _ = output.write_str("\x1b[Z");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::thread;
    use std::time::{Duration, Instant};

    fn shell(script: &str, rows: u16, cols: u16) -> EmbeddedTerminal {
        EmbeddedTerminal::spawn(&["/bin/sh".into(), "-c".into(), script.into()], rows, cols)
            .unwrap()
    }

    fn until(terminal: &mut EmbeddedTerminal, predicate: impl Fn(&EmbeddedTerminal) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            terminal.poll().unwrap();
            if predicate(terminal) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "terminal did not reach expected state: {}",
                terminal.parser.screen().contents()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn captured_stdout_drains_without_exposing_protocol_or_secret_input() {
        let _env = crate::test_support::env_read();
        let script = "stty -echo; printf 'Password: ' >&2; IFS= read -r password; \
                      [ \"$password\" = hidden-secret ] || exit 9; \
                      printf 'start\\n'; dd if=/dev/zero bs=65536 count=4 2>/dev/null; \
                      printf 'end\\n'; printf 'Accepted' >&2";
        let mut terminal = EmbeddedTerminal::spawn_captured(
            &["/bin/sh".into(), "-c".into(), script.into()],
            24,
            80,
        )
        .unwrap();
        until(&mut terminal, |terminal| {
            terminal.contents().contains("Password:")
        });
        terminal.paste("hidden-secret").unwrap();
        terminal
            .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        until(&mut terminal, |terminal| terminal.exit_status.is_some());
        assert_eq!(terminal.poll().unwrap(), Some(0));
        let output = terminal.captured_output();
        assert_eq!(output.len(), 6 + 4 * 65536 + 4);
        assert_eq!(&output[..6], b"start\n");
        assert_eq!(&output[output.len() - 4..], b"end\n");
        assert!(output[6..output.len() - 4].iter().all(|byte| *byte == 0));
        let visible = terminal.contents();
        assert!(visible.contains("Accepted"), "{visible}");
        assert!(!visible.contains("hidden-secret") && !visible.contains("start"));
    }

    #[test]
    fn captured_stdout_rejects_oversized_inventory() {
        let _env = crate::test_support::env_read();
        let mut terminal = EmbeddedTerminal::spawn_captured(
            &[
                "/bin/sh".into(),
                "-c".into(),
                "dd if=/dev/zero bs=65536 count=17 2>/dev/null".into(),
            ],
            24,
            80,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let error = loop {
            match terminal.poll() {
                Err(error) => break error,
                Ok(Some(code)) => panic!("oversized output accepted: {code}"),
                Ok(None) => {}
            }
            assert!(Instant::now() < deadline, "stdout capture stalled");
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(terminal.captured_output().len() <= crate::process::COMMAND_OUTPUT_LIMIT);
    }

    #[test]
    fn unicode_attributes_alternate_screen_and_cursor_render_as_cells() {
        let _env = crate::test_support::env_read();
        let mut terminal = shell("sleep 30", 3, 10);
        terminal.parser.process(
            "normal\x1b[?1049h\x1b[2J\x1b[H\x1b[1;3;4;38;2;12;34;56;48;5;123m界e\u{301}".as_bytes(),
        );
        let mut display = Terminal::new(TestBackend::new(10, 3)).unwrap();
        display
            .draw(|frame| {
                let area = frame.area();
                terminal.render(frame, area, true);
            })
            .unwrap();
        let buffer = display.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "界");
        assert_eq!(buffer[(2, 0)].symbol(), "e\u{301}");
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(12, 34, 56));
        assert_eq!(buffer[(0, 0)].bg, Color::Indexed(123));
        assert!(
            buffer[(0, 0)]
                .modifier
                .contains(Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED)
        );
        assert!(display.backend().cursor_visible());
        assert_eq!(display.backend().cursor_position(), (3, 0).into());
        display
            .draw(|frame| {
                let area = frame.area();
                terminal.render(frame, area, false);
            })
            .unwrap();
        assert!(!display.backend().cursor_visible());
        terminal.parser.process(b"\x1b[?25l");
        display
            .draw(|frame| {
                let area = frame.area();
                terminal.render(frame, area, true);
            })
            .unwrap();
        assert!(!display.backend().cursor_visible());
        terminal.parser.process(b"\x1b[?1049l");
        assert_eq!(terminal.parser.screen().contents(), "normal");
    }

    #[test]
    fn standard_and_modified_keys_use_terminal_protocol() {
        for (code, modifiers, application, expected) in [
            (
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                false,
                b"\x03".as_slice(),
            ),
            (
                KeyCode::Char('界'),
                KeyModifiers::ALT,
                false,
                "\x1b界".as_bytes(),
            ),
            (KeyCode::Up, KeyModifiers::NONE, true, b"\x1bOA"),
            (
                KeyCode::Up,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                true,
                b"\x1b[1;6A",
            ),
            (KeyCode::F(4), KeyModifiers::NONE, false, b"\x1bOS"),
            (KeyCode::F(12), KeyModifiers::ALT, false, b"\x1b[24;3~"),
            (KeyCode::F(24), KeyModifiers::NONE, false, b"\x1b[24;2~"),
            (KeyCode::F(63), KeyModifiers::NONE, false, b"\x1b[1;4R"),
            (KeyCode::Delete, KeyModifiers::NONE, false, b"\x1b[3~"),
            (KeyCode::BackTab, KeyModifiers::SHIFT, false, b"\x1b[Z"),
        ] {
            assert_eq!(
                encode_key(KeyEvent::new(code, modifiers), application).bytes(),
                expected
            );
        }
    }

    #[test]
    fn real_pty_answers_queries_and_delivers_bracketed_paste() {
        let _env = crate::test_support::env_read();
        // dd waits for an exact byte count; od makes received input observable
        // without having to interpret it as another escape sequence.
        let mut terminal = shell(
            r"stty raw -echo; printf '\033[?2004h\033[6n'; dd bs=1 count=6 2>/dev/null | od -An -tx1; printf '\r\nREADY\r\n'; dd bs=1 count=15 2>/dev/null | od -An -tx1",
            8,
            120,
        );
        until(&mut terminal, |t| {
            t.parser.screen().contents().contains("READY")
        });
        let response = terminal.parser.screen().contents();
        // BSD od changes padding when stdout is a terminal.
        assert_eq!(
            response.split_whitespace().collect::<String>(),
            "1b5b313b3152READY",
            "{response:?}",
        );
        terminal.paste("x\ny").unwrap();
        until(&mut terminal, |t| t.exit_status.is_some() && t.eof);
        let response = terminal.parser.screen().contents();
        assert!(
            response
                .split_whitespace()
                .collect::<String>()
                .contains("1b5b3230307e780a791b5b3230317e"),
            "{response:?}",
        );
    }

    #[test]
    fn query_replies_are_local_bounded_and_ignore_clipboard() {
        let mut parser = vt100::Parser::new_with_callbacks(
            NonZeroU16::new(5).unwrap(),
            NonZeroU16::new(9).unwrap(),
            0,
            Replies::default(),
        );
        parser.process(
            b"\x1b[3;4H\x1b[5n\x1b[6n\x1b[?6n\x1b[c\x1b[>c\x1b[18t\x1b]52;c;?\x07\x1b]2;title\x07",
        );
        assert_eq!(
            parser
                .callbacks()
                .pending
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            b"\x1b[0n\x1b[3;4R\x1b[?3;4R\x1b[?1;2c\x1b[>0;0;0c\x1b[8;5;9t"
        );
        assert_eq!(parser.screen().contents(), "");
    }

    #[test]
    fn streaming_utf8_and_oversized_control_strings_recover_without_rendering_controls() {
        let mut parser = vt100::Parser::new_with_callbacks(
            NonZeroU16::new(3).unwrap(),
            NonZeroU16::new(20).unwrap(),
            0,
            Replies::default(),
        );
        // An unfinished OSC/DCS must remain inert across poll-sized chunks.
        // vte without std bounds OSC to 1024 bytes; DCS is never retained.
        let chunk = [b'x'; 4096];
        for prefix in [b"\x1b]52;c;".as_slice(), b"\x1bPq".as_slice()] {
            parser.process(prefix);
            for _ in 0..512 {
                parser.process(&chunk);
            }
            assert_eq!(parser.screen().contents(), "");
            parser.process(b"\x1b\\");
        }
        parser.process(b"\x1b[");
        for _ in 0..512 {
            parser.process(&[b'9'; 4096]);
        }
        parser.process(b"\x18\x1b[H");
        for byte in "界e\u{301}".as_bytes() {
            parser.process(&[*byte]);
        }
        assert_eq!(parser.screen().contents(), "界e\u{301}");
        assert_eq!(parser.screen().cell(0, 2).unwrap().contents(), "e\u{301}");
        assert!(parser.callbacks().pending.is_empty());
    }

    #[test]
    fn resize_changes_kernel_size_and_clamps_zero() {
        let _env = crate::test_support::env_read();
        let mut terminal = shell("printf READY; read answer; stty size", 3, 20);
        until(&mut terminal, |t| {
            t.parser.screen().contents().contains("READY")
        });
        terminal.resize(0, 0).unwrap();
        assert_eq!(terminal.parser.screen().size(), dimensions(1, 1));
        terminal.resize(7, 31).unwrap();
        terminal
            .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        until(&mut terminal, |t| t.exit_status.is_some() && t.eof);
        assert!(terminal.parser.screen().contents().contains("7 31"));
    }

    #[test]
    fn exit_reaps_client_and_cleans_inherited_pty_descendants() {
        let _env = crate::test_support::env_read();
        let mut terminal = shell("sleep 30 & printf done; exit 7", 3, 20);
        until(&mut terminal, |t| t.exit_status.is_some() && t.eof);
        assert_eq!(terminal.poll().unwrap(), Some(7));
        assert_eq!(terminal.parser.screen().contents(), "done");
        assert_eq!(
            terminal
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .unwrap()
                .code(),
            Some(7)
        );
    }

    #[test]
    fn fresh_pty_is_cooked_and_drop_reaps_the_owned_client() {
        let _env = crate::test_support::env_read();
        let terminal = shell("sleep 30", 3, 20);
        // SAFETY: termios is a plain C value and master is a live PTY.
        let mut mode: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::tcgetattr(terminal.master_fd(), &mut mode) },
            0
        );
        assert_ne!(mode.c_lflag & libc::ICANON, 0);
        assert_ne!(mode.c_lflag & libc::ISIG, 0);
        assert_ne!(mode.c_lflag & libc::ECHO, 0);
        let pid = terminal.child.as_ref().unwrap().id() as libc::pid_t;
        drop(terminal);
        await_reaped(pid);
    }

    fn await_reaped(pid: libc::pid_t) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // Observe without stealing the background reaper's owned child.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == -1 {
                assert_eq!(
                    io::Error::last_os_error().raw_os_error(),
                    Some(libc::ECHILD)
                );
                return;
            }
            assert!(Instant::now() < deadline, "terminal child was not reaped");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn dropping_a_backpressured_client_does_not_block_and_reaps_it() {
        let _env = crate::test_support::env_read();
        let terminal = shell("while :; do printf '%01024d' 0; done", 3, 20);
        let pid = terminal.child.as_ref().unwrap().id() as libc::pid_t;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut pending = libc::pollfd {
                fd: terminal.master_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // Observe readiness without draining the child's output.
            assert!(unsafe { libc::poll(&mut pending, 1, 0) } >= 0);
            if pending.revents & libc::POLLIN != 0 {
                break;
            }
            assert!(Instant::now() < deadline, "client produced no output");
            thread::sleep(Duration::from_millis(5));
        }
        // The client cannot cooperate with shutdown while stopped.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        let (sent, received) = std::sync::mpsc::channel();
        thread::spawn(move || {
            drop(terminal);
            let _ = sent.send(());
        });
        received
            .recv_timeout(Duration::from_secs(2))
            .expect("terminal cleanup blocked");
        await_reaped(pid);
    }

    #[test]
    fn stopped_client_remains_connected_and_can_resume() {
        let _env = crate::test_support::env_read();
        let mut terminal = shell("kill -STOP $$; read answer; printf resumed", 3, 20);
        let pid = terminal.child.as_ref().unwrap().id() as libc::pid_t;
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // This fixture stops before reading input; include exit events so an
        // unexpected early failure is reported rather than waiting for a stop.
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
        assert_eq!(terminal.poll().unwrap(), None);
        assert_eq!(unsafe { libc::kill(pid, libc::SIGCONT) }, 0);
        terminal
            .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        until(&mut terminal, |t| t.exit_status.is_some() && t.eof);
        assert_eq!(terminal.exit_status, Some(0));
        assert_eq!(terminal.parser.screen().contents().trim(), "resumed");
    }

    #[test]
    fn oversized_paste_is_rejected_without_partial_framing() {
        let _env = crate::test_support::env_read();
        let mut terminal = shell("sleep 30", 3, 20);
        terminal.parser.process(b"\x1b[?2004h");
        let error = terminal.paste(&"x".repeat(INPUT_LIMIT)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(terminal.parser.callbacks().pending.is_empty());
    }
}
