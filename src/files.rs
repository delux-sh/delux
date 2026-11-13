use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

/// Readers reject larger files, so writers must never produce one.
pub(crate) const FILE_LIMIT: usize = 64 * 1024;

pub(crate) fn xdg_path(variable: &str, fallback: &str, filename: &str) -> Result<String, String> {
    let home = match std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        Some(home) => home,
        None => {
            let home = std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "$HOME is not defined".to_string())?;
            PathBuf::from(home).join(fallback)
        }
    };
    // A lossy conversion would silently read and write a different path.
    home.join("delux")
        .join(filename)
        .into_os_string()
        .into_string()
        .map_err(|_| format!("${variable} or $HOME is not valid UTF-8"))
}

/// Missing files are empty; existing files must be bounded UTF-8.
pub(crate) fn read_utf8(path: &str, label: &str) -> Result<Option<String>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open {path}: {error}")),
    };
    let mut raw = Vec::new();
    file.take((FILE_LIMIT + 1) as u64)
        .read_to_end(&mut raw)
        .map_err(|error| format!("read {path}: {error}"))?;
    if raw.len() > FILE_LIMIT {
        return Err(format!("{label} exceeds 64 KiB"));
    }
    String::from_utf8(raw)
        .map(Some)
        .map_err(|_| format!("{label} contains invalid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EnvVar, TempDir, env_write};
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn xdg_roots_must_be_absolute() {
        let _env = env_write();
        let root = TempDir::new();
        let _home = EnvVar::set("HOME", root.path());
        for (variable, fallback, filename) in [
            ("XDG_CONFIG_HOME", ".config", "cli.json"),
            ("XDG_STATE_HOME", ".local/state", "state.json"),
        ] {
            for invalid in ["", ".", "relative", "~/xdg"] {
                let _xdg = EnvVar::set(variable, invalid);
                let expected = root.path().join(fallback).join("delux").join(filename);
                assert_eq!(
                    xdg_path(variable, fallback, filename).unwrap(),
                    expected.to_str().unwrap()
                );
                let _no_home = EnvVar::remove("HOME");
                assert!(xdg_path(variable, fallback, filename).is_err());
            }
            let _no_home = EnvVar::remove("HOME");
            let _xdg = EnvVar::set(variable, "/");
            assert_eq!(
                xdg_path(variable, fallback, filename).unwrap(),
                format!("/delux/{filename}")
            );
        }
    }

    #[test]
    fn non_utf8_roots_are_refused_rather_than_altered() {
        let _env = env_write();
        let invalid = OsStr::from_bytes(b"/tmp/delux-\xff");
        let _home = EnvVar::set("HOME", invalid);
        let _no_xdg = EnvVar::remove("XDG_STATE_HOME");
        assert!(xdg_path("XDG_STATE_HOME", ".local/state", "state.json").is_err());
        let _valid_home = EnvVar::set("HOME", "/tmp");
        let _xdg = EnvVar::set("XDG_STATE_HOME", invalid);
        assert!(xdg_path("XDG_STATE_HOME", ".local/state", "state.json").is_err());
    }
}
