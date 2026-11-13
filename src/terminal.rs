use std::os::fd::RawFd;

pub fn is_terminal(fd: RawFd) -> bool {
    // SAFETY: isatty only inspects the supplied descriptor and accepts invalid fds.
    unsafe { libc::isatty(fd) == 1 }
}

pub struct TerminalState(libc::termios);

pub fn terminal_state(fd: RawFd) -> Option<TerminalState> {
    // SAFETY: termios contains only integer fields; zero is a valid initial value.
    let mut state: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: the pointer has initialized storage for termios. Some platforms
    // write only part of c_cc, so untouched entries must already be initialized.
    if unsafe { libc::tcgetattr(fd, &mut state) } == 0 {
        Some(TerminalState(state))
    } else {
        None
    }
}

pub fn restore_terminal(fd: RawFd, state: &TerminalState) {
    // SAFETY: the termios pointer is initialized and valid for this call.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &state.0) };
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use super::*;
    use crate::test_support::open_pty;

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut descriptors = [-1; 2];
        // SAFETY: the array provides storage for the two output descriptors.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: pipe returned two new descriptors, now owned independently.
        unsafe {
            (
                OwnedFd::from_raw_fd(descriptors[0]),
                OwnedFd::from_raw_fd(descriptors[1]),
            )
        }
    }

    #[test]
    fn terminal_state_rejects_pipes_and_restores_pty() {
        let (reader, _writer) = pipe();
        assert!(!is_terminal(reader.as_raw_fd()));
        assert!(terminal_state(reader.as_raw_fd()).is_none());
        let (_master, slave) = open_pty(24, 80);
        assert!(is_terminal(slave.as_raw_fd()));
        let before = terminal_state(slave.as_raw_fd()).unwrap();
        let mut raw = terminal_state(slave.as_raw_fd()).unwrap();
        // SAFETY: raw contains an initialized termios value.
        unsafe { libc::cfmakeraw(&mut raw.0) };
        restore_terminal(slave.as_raw_fd(), &raw);
        restore_terminal(slave.as_raw_fd(), &before);
        let after = terminal_state(slave.as_raw_fd()).unwrap();
        assert_eq!(before.0.c_iflag, after.0.c_iflag);
        assert_eq!(before.0.c_oflag, after.0.c_oflag);
        assert_eq!(before.0.c_cflag, after.0.c_cflag);
        assert_eq!(
            before.0.c_lflag & !libc::PENDIN,
            after.0.c_lflag & !libc::PENDIN
        );
        assert_eq!(before.0.c_cc, after.0.c_cc);
    }
}
