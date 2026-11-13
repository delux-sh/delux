//! Shared test fixtures. Tests run as threads in one process, so environment
//! changes must not overlap with tests that read the environment or start
//! subprocesses (which inherit it).

use std::ffi::{OsStr, OsString};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

static ENVIRONMENT: RwLock<()> = RwLock::new(());

/// Hold while reading the environment or running subprocesses.
pub fn env_read() -> RwLockReadGuard<'static, ()> {
    ENVIRONMENT
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Hold while changing the environment; pair with [`EnvVar`] for restoration.
pub fn env_write() -> RwLockWriteGuard<'static, ()> {
    ENVIRONMENT
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Sets or removes one variable and restores the previous value on drop.
/// Callers must hold [`env_write`] for the guard's whole lifetime.
pub struct EnvVar {
    name: OsString,
    previous: Option<OsString>,
}

impl EnvVar {
    pub fn set(name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        let name = name.as_ref().to_owned();
        let previous = std::env::var_os(&name);
        // SAFETY: the caller holds the environment write lock, so no other test
        // thread reads or writes the environment concurrently.
        unsafe { std::env::set_var(&name, value) };
        Self { name, previous }
    }

    pub fn remove(name: impl AsRef<OsStr>) -> Self {
        let name = name.as_ref().to_owned();
        let previous = std::env::var_os(&name);
        // SAFETY: see `set`.
        unsafe { std::env::remove_var(&name) };
        Self { name, previous }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        // SAFETY: see `set`.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(&self.name, value),
                None => std::env::remove_var(&self.name),
            }
        }
    }
}

/// A uniquely named directory removed on drop. Rooted in /tmp by default,
/// tmux's own base: macOS caps Unix socket paths at 103 bytes, which its
/// per-user $TMPDIR plus nested names can exceed.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let template = std::ffi::CString::new("/tmp/delux-test-XXXXXX").unwrap();
        let mut bytes = template.into_bytes_with_nul();
        // SAFETY: `bytes` is a writable NUL-terminated template.
        let result = unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) };
        assert!(
            !result.is_null(),
            "mkdtemp: {}",
            std::io::Error::last_os_error()
        );
        bytes.pop();
        Self(PathBuf::from(String::from_utf8(bytes).unwrap()))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Opens a pseudo-terminal, returning (master, slave).
#[allow(clippy::unnecessary_mut_passed)] // Required by the macOS signature.
pub fn open_pty(rows: u16, columns: u16) -> (OwnedFd, OwnedFd) {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: the out-pointers are valid; name and termios may be null. macOS
    // declares the termios/winsize pointers mutable and Linux const, so pass
    // mutable pointers, which coerce to either.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    assert_eq!(result, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: openpty returned two new descriptors that we now own.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}
