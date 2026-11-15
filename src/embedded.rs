//! A private PTY for one SSH/tmux client. Nothing received from the child is
//! written directly to Delux's terminal: only parsed cells reach the renderer.

use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::io::{self, Read};
use std::num::NonZeroU16;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, waitid};
use unicode_width::UnicodeWidthStr;

const INPUT_LIMIT: usize = 1024 * 1024;
const IO_BUDGET: usize = 64 * 1024;
const IO_CALLS: usize = 32;
const SYNC_TIMEOUT: Duration = Duration::from_secs(1);
// tmux can hide the cursor before emitting its synchronized-frame marker.
const CURSOR_HIDE_GRACE: Duration = Duration::from_millis(50);

#[derive(Default)]
struct RenderedScreen {
    buffer: Buffer,
    cursor: Option<(u16, u16)>,
    hide_since: Option<Instant>,
}

impl RenderedScreen {
    fn capture(&mut self, screen: &vt100::Screen) {
        let (rows, cols) = screen.size();
        self.buffer.resize(Rect::new(0, 0, cols.get(), rows.get()));
        for row in 0..rows.get() {
            for col in 0..cols.get() {
                let target = &mut self.buffer[(col, row)];
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
                // Reset continuation cells, including stale wide characters.
                if cell.has_contents()
                    && !cell.is_wide_continuation()
                    && (!cell.is_wide() || col + 1 < cols.get())
                {
                    target.set_symbol(cell.contents());
                }
            }
        }
        self.cursor = (!screen.hide_cursor()).then(|| screen.cursor_position());
        self.hide_since = None;
    }
}

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
    synchronized: Option<Instant>,
    display: RenderedScreen,
    captured_frame: bool,
}

impl Replies {
    fn synchronizing(&self) -> bool {
        self.synchronized
            .is_some_and(|started| started.elapsed() < SYNC_TIMEOUT)
    }

    fn set_synchronized(&mut self, enabled: bool, screen: &vt100::Screen) {
        if enabled {
            // Repeated begins must not postpone the safety deadline.
            self.synchronized.get_or_insert_with(Instant::now);
        } else if self.synchronized.take().is_some() && !self.captured_frame {
            // Keep a complete frame even if another begins in the same read.
            // Bound capture work: a stream of tiny frames must not multiply
            // the PTY read budget by thousands of whole-screen copies.
            self.display.capture(screen);
            self.captured_frame = true;
        }
    }

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
        if i1 == Some(b'?') {
            if i2 == Some(b'$') && command == 'p' && params == [&[2026][..]] {
                self.reply(if self.synchronizing() {
                    b"\x1b[?2026;1$y"
                } else {
                    b"\x1b[?2026;2$y"
                });
                return;
            }
            if i2.is_none() && params.iter().any(|p| *p == [2026]) {
                match command {
                    'h' => self.set_synchronized(true, screen),
                    'l' => self.set_synchronized(false, screen),
                    _ => {}
                }
                return;
            }
        }
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

// tmux 3.2 uses DCS =1s/=2s rather than DEC 2026. vt100 ignores DCS, so
// recognize these exact markers across reads without buffering their payload.
#[derive(Default)]
struct DcsSync {
    matched: u8,
    enabled: bool,
}

impl DcsSync {
    fn feed(&mut self, byte: u8) -> Option<bool> {
        match (self.matched, byte) {
            (0, 0x1b) | (1, b'P') | (2, b'=') | (4, b's') | (5, 0x1b) => {
                self.matched += 1;
            }
            (3, b'1' | b'2') => {
                self.enabled = byte == b'1';
                self.matched += 1;
            }
            (6, b'\\') => {
                self.matched = 0;
                return Some(self.enabled);
            }
            _ => self.matched = u8::from(byte == 0x1b),
        }
        None
    }
}

pub(crate) struct EmbeddedTerminal {
    master: Option<OwnedFd>,
    child: Option<Child>,
    parser: vt100::Parser<Replies>,
    dcs_sync: DcsSync,
    display_dirty: bool,
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
                rustix::process::setsid()?;
                rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
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
            dcs_sync: DcsSync::default(),
            display_dirty: true,
            exit_status: None,
            eof: false,
            output,
        };
        if let Some(output) = &terminal.output {
            // `terminal` owns the child already, so failure still tears it down.
            nonblocking(&output.stream)?;
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
            // NOWAIT reserves this child's PID until group cleanup and reaping.
            let pid = Pid::from_child(self.child.as_ref().expect("owned terminal child"));
            let info = match waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            ) {
                Ok(info) => info,
                Err(rustix::io::Errno::INTR) => None,
                Err(error) => return Err(error.into()),
            };
            if info.is_some_and(|info| info.exited() || info.killed() || info.dumped()) {
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

    fn process_output(&mut self, bytes: &[u8]) {
        self.parser.callbacks_mut().captured_frame = false;
        let mut start = 0;
        for (index, &byte) in bytes.iter().enumerate() {
            if let Some(enabled) = self.dcs_sync.feed(byte) {
                self.parser.process(&bytes[start..=index]);
                let (screen, callbacks) = self.parser.screen_and_callbacks_mut();
                callbacks.set_synchronized(enabled, screen);
                start = index + 1;
            }
        }
        self.parser.process(&bytes[start..]);
        self.display_dirty = true;
    }

    fn read_output(&mut self) -> io::Result<()> {
        if self.eof {
            return Ok(());
        }
        let mut buffer = [0; 4096];
        let mut total = 0;
        for _ in 0..IO_CALLS {
            match rustix::io::read(self.master_fd(), &mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(());
                }
                Ok(count) => {
                    self.process_output(&buffer[..count]);
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
                }
                Err(rustix::io::Errno::IO) => {
                    // Linux reports a closed PTY slave as EIO instead of EOF.
                    self.eof = true;
                    return Ok(());
                }
                Err(rustix::io::Errno::AGAIN) => return Ok(()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn flush_input(&mut self) -> io::Result<()> {
        if self.eof || self.exit_status.is_some() {
            return Ok(());
        }
        let master = self.master.as_ref().expect("open terminal PTY");
        let queue = &mut self.parser.callbacks_mut().pending;
        let mut total = 0;
        for _ in 0..IO_CALLS {
            let (first, second) = queue.as_slices();
            let bytes = if first.is_empty() { second } else { first };
            let bytes = &bytes[..bytes.len().min(IO_BUDGET - total)];
            if bytes.is_empty() {
                break;
            }
            match rustix::io::write(master, bytes) {
                Ok(0) => break,
                Ok(count) => {
                    queue.drain(..count);
                    total += count;
                    if total == IO_BUDGET {
                        break;
                    }
                }
                Err(rustix::io::Errno::IO | rustix::io::Errno::PIPE) => {
                    queue.clear();
                    break;
                }
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) -> io::Result<()> {
        let (rows, cols) = dimensions(rows, cols);
        if self.parser.screen().size() == (rows, cols) {
            return Ok(());
        }
        let size = rustix::termios::Winsize {
            ws_row: rows.get(),
            ws_col: cols.get(),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        rustix::termios::tcsetwinsize(self.master_fd(), size)?;
        self.parser.set_size(rows, cols);
        self.parser.callbacks_mut().synchronized = None;
        self.display_dirty = true;
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

    /// Forward wheel input only when the child requested a compatible mouse
    /// protocol. Coordinates are zero-based positions inside this PTY.
    pub(crate) fn scroll(&mut self, event: MouseEvent) -> io::Result<bool> {
        let mut button = match event.kind {
            MouseEventKind::ScrollUp => 64,
            MouseEventKind::ScrollDown => 65,
            MouseEventKind::ScrollLeft => 66,
            MouseEventKind::ScrollRight => 67,
            _ => return Ok(false),
        };
        let screen = self.parser.screen();
        if matches!(
            screen.mouse_protocol_mode(),
            vt100::MouseProtocolMode::None | vt100::MouseProtocolMode::Press
        ) {
            return Ok(false);
        }
        let (rows, cols) = screen.size();
        if event.row >= rows.get() || event.column >= cols.get() {
            return Ok(false);
        }
        if event.modifiers.contains(KeyModifiers::SHIFT) {
            button |= 4;
        }
        if event.modifiers.contains(KeyModifiers::ALT) {
            button |= 8;
        }
        if event.modifiers.contains(KeyModifiers::CONTROL) {
            button |= 16;
        }
        let mut sequence = Sequence::new();
        match screen.mouse_protocol_encoding() {
            vt100::MouseProtocolEncoding::Sgr => {
                let _ = write!(
                    sequence,
                    "\x1b[<{button};{};{}M",
                    u32::from(event.column) + 1,
                    u32::from(event.row) + 1
                );
            }
            vt100::MouseProtocolEncoding::Default => {
                // The one-based position plus 32 must fit in one byte.
                if event.column > 222 || event.row > 222 {
                    return Ok(false);
                }
                sequence.bytes[..6].copy_from_slice(&[
                    0x1b,
                    b'[',
                    b'M',
                    button + 32,
                    (event.column + 33) as u8,
                    (event.row + 33) as u8,
                ]);
                sequence.len = 6;
            }
            vt100::MouseProtocolEncoding::Utf8 => {
                // Xterm's 1005 protocol supports positions through 2015,
                // encoded in at most two UTF-8 bytes after adding 32.
                if event.column > 2014 || event.row > 2014 {
                    return Ok(false);
                }
                let _ = sequence.write_str("\x1b[M");
                let _ = sequence.write_char(char::from(button + 32));
                for coordinate in [event.column, event.row] {
                    let _ = sequence.write_char(
                        char::from_u32(u32::from(coordinate) + 33).expect("bounded coordinate"),
                    );
                }
            }
        }
        self.parser.callbacks_mut().enqueue(sequence.bytes())?;
        self.flush_input()?;
        Ok(true)
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

    fn refresh_display(&mut self) {
        if !self.display_dirty || self.parser.callbacks().synchronizing() {
            return;
        }
        let (screen, callbacks) = self.parser.screen_and_callbacks_mut();
        if screen.hide_cursor() && callbacks.display.cursor.is_some() {
            let since = callbacks
                .display
                .hide_since
                .get_or_insert_with(Instant::now);
            if since.elapsed() < CURSOR_HIDE_GRACE {
                return;
            }
        }
        callbacks.display.capture(screen);
        self.display_dirty = false;
    }

    pub(crate) fn render(&mut self, frame: &mut Frame<'_>, area: Rect, focused: bool) {
        self.refresh_display();
        let display = &self.parser.callbacks().display;
        let area = area.intersection(frame.area());
        let buffer = frame.buffer_mut();
        for row in 0..area.height {
            for col in 0..area.width {
                let target = &mut buffer[(area.x + col, area.y + row)];
                if let Some(cell) = display.buffer.cell((col, row)) {
                    target.clone_from(cell);
                    if col + 1 == area.width && target.symbol().width() > 1 {
                        target.set_symbol(" ");
                    }
                } else {
                    target.reset();
                }
            }
        }
        if focused
            && let Some((row, col)) = display.cursor
            && row < area.height
            && col < area.width
        {
            frame.set_cursor_position((area.x + col, area.y + row));
        }
    }

    fn master_fd(&self) -> BorrowedFd<'_> {
        self.master.as_ref().expect("open terminal PTY").as_fd()
    }

    fn kill_group(&self) {
        // Only called before this Child has been reaped, so its PID
        // cannot be reused. setsid made that PID the client's process group.
        // A ControlPersist master daemonizes into a different session/group;
        // tmux's server likewise remains independent of this attached client.
        let pid = Pid::from_child(self.child.as_ref().expect("owned terminal child"));
        let _ = rustix::process::kill_process_group(pid, Signal::KILL);
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

fn open_pty(rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let size = rustix::termios::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // None requests cooked platform defaults, not the picker's raw stdin.
    // openpty returns owned descriptors with CLOEXEC set on both endpoints.
    let pty = rustix_openpty::openpty(None, Some(&size))?;
    nonblocking(&pty.controller)?;
    Ok((pty.controller, pty.user))
}

fn nonblocking(fd: impl AsFd) -> io::Result<()> {
    let fd = fd.as_fd();
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)?;
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
    use std::os::fd::AsRawFd;
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
        terminal.process_output(
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
        terminal.process_output(b"\x1b[?1049l");
        assert_eq!(terminal.parser.screen().contents(), "normal");
    }

    #[test]
    fn synchronized_output_keeps_cells_and_cursor_until_frame_end() {
        let _env = crate::test_support::env_read();
        let mut terminal = shell("sleep 30", 3, 10);
        let mut display = Terminal::new(TestBackend::new(10, 3)).unwrap();
        let render = |terminal: &mut EmbeddedTerminal, display: &mut Terminal<TestBackend>| {
            display
                .draw(|frame| terminal.render(frame, frame.area(), true))
                .unwrap();
        };
        terminal.process_output(b"old");
        render(&mut terminal, &mut display);
        terminal.process_output(b"\x1b[?20");
        terminal.process_output(b"26h\x1b[?25l\x1b[Hnew\x1b[?2026$p");
        assert_eq!(terminal.parser.screen().contents(), "new");
        assert_eq!(
            terminal
                .parser
                .callbacks_mut()
                .pending
                .drain(..)
                .collect::<Vec<_>>(),
            b"\x1b[?2026;1$y"
        );
        render(&mut terminal, &mut display);
        assert_eq!(display.backend().buffer()[(0, 0)].symbol(), "o");
        assert!(display.backend().cursor_visible());
        assert_eq!(display.backend().cursor_position(), (3, 0).into());

        terminal.process_output(b"\x1b[2;3H\x1b[?25h\x1b[?2026l\x1b[?2026$p");
        render(&mut terminal, &mut display);
        assert_eq!(display.backend().buffer()[(0, 0)].symbol(), "n");
        assert!(display.backend().cursor_visible());
        assert_eq!(display.backend().cursor_position(), (2, 1).into());
        assert_eq!(
            terminal
                .parser
                .callbacks_mut()
                .pending
                .drain(..)
                .collect::<Vec<_>>(),
            b"\x1b[?2026;2$y"
        );
    }

    #[test]
    fn synchronized_output_timeout_and_resize_do_not_leave_a_frozen_pane() {
        let _env = crate::test_support::env_read();
        let mut terminal = shell("sleep 30", 3, 10);
        let mut display = Terminal::new(TestBackend::new(10, 3)).unwrap();
        terminal.process_output(b"\x1b[?2026h\x1b[?25lnew");
        terminal.parser.callbacks_mut().synchronized = Some(Instant::now() - SYNC_TIMEOUT);
        terminal.process_output(b"\x1b[?2026h");
        display
            .draw(|frame| terminal.render(frame, frame.area(), true))
            .unwrap();
        assert_eq!(display.backend().buffer()[(0, 0)].symbol(), "n");
        assert!(!display.backend().cursor_visible());

        terminal.process_output(b"\x1b[?2026l\x1b[?2026h\x1b[Hresized\x1b[?25h");
        terminal.resize(4, 12).unwrap();
        display
            .draw(|frame| terminal.render(frame, frame.area(), true))
            .unwrap();
        assert_eq!(display.backend().buffer()[(0, 0)].symbol(), "r");
        assert!(display.backend().cursor_visible());
        assert_eq!(display.backend().cursor_position(), (7, 0).into());
    }

    #[test]
    fn synchronized_output_accepts_split_tmux_dcs_markers() {
        let _env = crate::test_support::env_read();
        let mut terminal = shell("sleep 30", 3, 10);
        terminal.process_output(b"old");
        terminal.refresh_display();
        for byte in b"\x1bP=1s\x1b\\" {
            terminal.process_output(&[*byte]);
        }
        terminal.process_output(b"\x1b[?25l\x1b[Hnew");
        terminal.refresh_display();
        assert_eq!(
            terminal.parser.callbacks().display.buffer[(0, 0)].symbol(),
            "o"
        );
        assert_eq!(terminal.parser.callbacks().display.cursor, Some((0, 3)));
        for byte in b"\x1b[?25h\x1bP=2s\x1b\\" {
            terminal.process_output(&[*byte]);
        }
        terminal.refresh_display();
        assert_eq!(
            terminal.parser.callbacks().display.buffer[(0, 0)].symbol(),
            "n"
        );
        assert_eq!(terminal.parser.callbacks().display.cursor, Some((0, 3)));

        // Mixed markers in a single read retain their wire order.
        terminal.process_output(b"\x1bP=1s\x1b\\\x1b[?2026l");
        assert!(!terminal.parser.callbacks().synchronizing());
        terminal.process_output(b"\x1b[?2026h\x1bP=2s\x1b\\");
        assert!(!terminal.parser.callbacks().synchronizing());
    }

    #[test]
    fn cursor_preludes_coalesce_without_losing_completed_frames_or_real_hides() {
        let _env = crate::test_support::env_read();
        let mut terminal = shell("sleep 30", 3, 10);
        let mut display = Terminal::new(TestBackend::new(10, 3)).unwrap();
        let render = |terminal: &mut EmbeddedTerminal, display: &mut Terminal<TestBackend>| {
            display
                .draw(|frame| terminal.render(frame, frame.area(), true))
                .unwrap();
        };
        terminal.process_output(b"old");
        render(&mut terminal, &mut display);
        terminal.process_output(b"\x1b[?25l");
        render(&mut terminal, &mut display);
        assert!(display.backend().cursor_visible());
        assert_eq!(display.backend().cursor_position(), (3, 0).into());

        terminal.process_output(
            b"\x1b[?2026h\x1b[Hnew\x1b[?25h\x1b[?2026l\x1b[?25l\x1b[?2026h\x1b[Hpartial",
        );
        render(&mut terminal, &mut display);
        assert_eq!(display.backend().buffer()[(0, 0)].symbol(), "n");
        assert!(display.backend().cursor_visible());
        terminal.process_output(b"\x1b[?25h\x1b[?2026l\x1b[?25l");
        render(&mut terminal, &mut display);
        assert_eq!(display.backend().buffer()[(0, 0)].symbol(), "p");
        assert!(display.backend().cursor_visible());

        terminal.parser.callbacks_mut().display.hide_since =
            Some(Instant::now() - CURSOR_HIDE_GRACE);
        render(&mut terminal, &mut display);
        assert!(!display.backend().cursor_visible());
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

    fn wheel_receiver(modes: &str, byte_count: usize, rows: u16, cols: u16) -> EmbeddedTerminal {
        let script = format!(
            "stty raw -echo; stty opost onlcr; printf '{modes}READY\\r\\n'; \
             dd bs=1 count={byte_count} 2>/dev/null | od -An -tx1"
        );
        let mut terminal = shell(&script, rows, cols);
        until(&mut terminal, |t| t.contents().contains("READY"));
        terminal
    }

    fn wheel(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn assert_received(terminal: &mut EmbeddedTerminal, expected: &[u8]) {
        until(terminal, |t| t.exit_status.is_some() && t.eof);
        assert_eq!(terminal.exit_status, Some(0));
        let mut hex = String::from("READY");
        for byte in expected {
            write!(hex, "{byte:02x}").unwrap();
        }
        assert_eq!(
            terminal.contents().split_whitespace().collect::<String>(),
            hex
        );
    }

    #[test]
    fn real_pty_receives_sgr_wheels_with_modifiers_and_unchanged_arrows() {
        let _env = crate::test_support::env_read();
        for mode in [1000, 1002, 1003] {
            let expected = b"\x1b[<64;1;1M\x1b[<69;2;3M\x1b[<74;4;5M\x1b[<95;120;8M\x1b[A\x1b[Bq";
            let mut terminal =
                wheel_receiver(&format!("\x1b[?{mode}h\x1b[?1006h"), expected.len(), 8, 120);
            for (kind, column, row, modifiers) in [
                (MouseEventKind::ScrollUp, 0, 0, KeyModifiers::NONE),
                (MouseEventKind::ScrollDown, 1, 2, KeyModifiers::SHIFT),
                (MouseEventKind::ScrollLeft, 3, 4, KeyModifiers::ALT),
                (
                    MouseEventKind::ScrollRight,
                    119,
                    7,
                    KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
                ),
            ] {
                let mut event = wheel(kind, column, row);
                event.modifiers = modifiers;
                assert!(terminal.scroll(event).unwrap());
            }
            for kind in [
                MouseEventKind::Moved,
                MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
                MouseEventKind::Up(ratatui::crossterm::event::MouseButton::Left),
                MouseEventKind::Drag(ratatui::crossterm::event::MouseButton::Left),
            ] {
                assert!(!terminal.scroll(wheel(kind, 0, 0)).unwrap());
            }
            for (column, row) in [(120, 0), (0, 8), (u16::MAX, u16::MAX)] {
                assert!(
                    !terminal
                        .scroll(wheel(MouseEventKind::ScrollUp, column, row))
                        .unwrap()
                );
            }
            for code in [KeyCode::Up, KeyCode::Down, KeyCode::Char('q')] {
                terminal
                    .send_key(KeyEvent::new(code, KeyModifiers::NONE))
                    .unwrap();
            }
            assert_received(&mut terminal, expected);
        }
    }

    #[test]
    fn real_pty_ignores_wheels_without_reporting_and_after_it_is_disabled() {
        let _env = crate::test_support::env_read();
        for modes in ["", "\x1b[?9h", "\x1b[?1006h", "\x1b[?9h\x1b[?1006h"] {
            let expected = b"\x1b[Aq";
            let mut terminal = wheel_receiver(modes, expected.len(), 8, 120);
            for kind in [
                MouseEventKind::ScrollUp,
                MouseEventKind::ScrollDown,
                MouseEventKind::ScrollLeft,
                MouseEventKind::ScrollRight,
            ] {
                assert!(!terminal.scroll(wheel(kind, 0, 0)).unwrap());
            }
            terminal
                .send_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
                .unwrap();
            terminal.paste("q").unwrap();
            assert_received(&mut terminal, expected);
        }

        let mut terminal = shell(
            r"stty raw -echo; printf '\033[?1000h\033[?1006hREADY\r\n'; \
              dd bs=1 count=10 2>/dev/null | od -An -tx1; \
              printf '\033[?1000l\r\nOFF\r\n'; \
              dd bs=1 count=4 2>/dev/null | od -An -tx1",
            8,
            120,
        );
        until(&mut terminal, |t| t.contents().contains("READY"));
        assert!(
            terminal
                .scroll(wheel(MouseEventKind::ScrollUp, 0, 0))
                .unwrap()
        );
        until(&mut terminal, |t| t.contents().contains("OFF"));
        assert!(
            !terminal
                .scroll(wheel(MouseEventKind::ScrollDown, 0, 0))
                .unwrap()
        );
        terminal
            .send_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .unwrap();
        terminal.paste("q").unwrap();
        until(&mut terminal, |t| t.exit_status.is_some() && t.eof);
        assert_eq!(terminal.exit_status, Some(0));
        assert_eq!(
            terminal.contents().split_whitespace().collect::<String>(),
            "READY1b5b3c36343b313b314dOFF1b5b4171"
        );
    }

    #[test]
    fn real_pty_wheel_encoding_boundaries_never_wrap_coordinates() {
        let _env = crate::test_support::env_read();
        // Each accepted packet is followed by a sentinel. Rejected wheel
        // events must not insert bytes ahead of that sentinel, including arrows.
        for (encoding, rows, cols, column, row, invalid_column, invalid_row, packet) in [
            ("", 8, 225, 222, 0, 223, 0, b"\x1b[M\x7f\xff!".as_slice()),
            ("", 225, 120, 0, 222, 0, 223, b"\x1b[M\x7f!\xff"),
            (
                "\x1b[?1005h",
                8,
                2016,
                2014,
                0,
                2015,
                0,
                b"\x1b[M\x7f\xdf\xbf!",
            ),
            (
                "\x1b[?1005h",
                2016,
                120,
                0,
                2014,
                0,
                2015,
                b"\x1b[M\x7f!\xdf\xbf",
            ),
            ("\x1b[?1006h", 8, 300, 299, 7, 300, 7, b"\x1b[<95;300;8M"),
        ] {
            let mut terminal = wheel_receiver(
                &format!("\x1b[?1000h{encoding}"),
                packet.len() + 1,
                rows,
                cols,
            );
            let mut event = wheel(MouseEventKind::ScrollRight, column, row);
            event.modifiers = KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL;
            assert!(terminal.scroll(event).unwrap());
            event.column = invalid_column;
            event.row = invalid_row;
            assert!(!terminal.scroll(event).unwrap());
            terminal.paste("q").unwrap();
            let mut expected = packet.to_vec();
            expected.push(b'q');
            assert_received(&mut terminal, &expected);
        }

        let expected = b"\x1b[M`\x7f\xc2\x80q";
        let mut terminal = wheel_receiver("\x1b[?1000h\x1b[?1005h", expected.len(), 96, 120);
        assert!(
            terminal
                .scroll(wheel(MouseEventKind::ScrollUp, 94, 95))
                .unwrap()
        );
        terminal.paste("q").unwrap();
        assert_received(&mut terminal, expected);
    }

    #[test]
    fn wheel_queue_overflow_rejects_the_entire_packet_and_recovers() {
        let _env = crate::test_support::env_read();
        let expected = b"\x1b[<64;1;1Mq";
        let mut terminal = wheel_receiver("\x1b[?1000h\x1b[?1006h", expected.len(), 8, 120);
        terminal
            .parser
            .callbacks_mut()
            .pending
            .extend(std::iter::repeat_n(b'x', INPUT_LIMIT - 5));
        let error = terminal
            .scroll(wheel(MouseEventKind::ScrollUp, 0, 0))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let pending = &mut terminal.parser.callbacks_mut().pending;
        assert_eq!(pending.len(), INPUT_LIMIT - 5);
        assert!(pending.iter().all(|byte| *byte == b'x'));
        pending.clear();
        assert!(
            terminal
                .scroll(wheel(MouseEventKind::ScrollUp, 0, 0))
                .unwrap()
        );
        terminal.paste("q").unwrap();
        assert_received(&mut terminal, expected);
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
            unsafe { libc::tcgetattr(terminal.master_fd().as_raw_fd(), &mut mode) },
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
            let mut pending = [rustix::event::PollFd::from_borrowed_fd(
                terminal.master_fd(),
                rustix::event::PollFlags::IN,
            )];
            // Observe readiness without draining the child's output.
            rustix::event::poll(&mut pending, Some(&rustix::event::Timespec::default())).unwrap();
            if pending[0].revents().contains(rustix::event::PollFlags::IN) {
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
        terminal.process_output(b"\x1b[?2004h");
        let error = terminal.paste(&"x".repeat(INPUT_LIMIT)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(terminal.parser.callbacks().pending.is_empty());
    }
}
