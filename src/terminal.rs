use std::os::fd::{AsFd, AsRawFd};

pub fn is_terminal(fd: impl AsFd) -> bool {
    rustix::termios::isatty(fd)
}

pub struct TerminalState(libc::termios);

pub fn terminal_state(fd: impl AsFd) -> Option<TerminalState> {
    // SAFETY: termios contains only integer fields; zero is a valid initial value.
    let mut state: libc::termios = unsafe { std::mem::zeroed() };
    // Keep this initialized libc representation: rustix 1.1.6's Darwin
    // tcgetattr uses uninitialized storage, including any unwritten c_cc tail.
    // SAFETY: the pointer has initialized storage for termios. Some platforms
    // write only part of c_cc, so untouched entries must already be initialized.
    if unsafe { libc::tcgetattr(fd.as_fd().as_raw_fd(), &mut state) } == 0 {
        Some(TerminalState(state))
    } else {
        None
    }
}

pub fn restore_terminal(fd: impl AsFd, state: &TerminalState) {
    // SAFETY: the borrowed fd is live and state is the initialized libc termios
    // captured above; it cannot be passed to rustix without a layout conversion.
    unsafe { libc::tcsetattr(fd.as_fd().as_raw_fd(), libc::TCSANOW, &state.0) };
}

#[cfg(test)]
mod tests {
    use std::os::fd::{FromRawFd, OwnedFd};

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
        assert!(!is_terminal(&reader));
        assert!(terminal_state(&reader).is_none());
        let (_master, slave) = open_pty(24, 80);
        assert!(is_terminal(&slave));
        let before = terminal_state(&slave).unwrap();
        let mut raw = terminal_state(&slave).unwrap();
        // SAFETY: raw contains an initialized termios value.
        unsafe { libc::cfmakeraw(&mut raw.0) };
        restore_terminal(&slave, &raw);
        restore_terminal(&slave, &before);
        let after = terminal_state(&slave).unwrap();
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
