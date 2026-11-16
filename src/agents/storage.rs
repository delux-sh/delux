use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) const FILE_LIMIT: usize = 1024 * 1024;

#[derive(Debug)]
pub(super) struct Update {
    pub changed: bool,
    pub backup: Option<PathBuf>,
}

pub(super) fn read(path: &Path) -> Result<Option<String>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let mut raw = Vec::new();
    file.take((FILE_LIMIT + 1) as u64)
        .read_to_end(&mut raw)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if raw.len() > FILE_LIMIT {
        return Err(format!(
            "{} exceeds the 1 MiB agent configuration limit; nothing was changed",
            path.display()
        ));
    }
    String::from_utf8(raw)
        .map(Some)
        .map_err(|_| format!("{} contains invalid UTF-8", path.display()))
}

// Lock the directory inode: atomic replacement cannot change that inode, and
// no persistent sidecar is left in another program's configuration directory.
pub(super) fn update(
    path: &Path,
    backup_existing: bool,
    change: impl FnOnce(Option<&str>) -> Result<Option<String>, String>,
) -> Result<Update, String> {
    let target = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)
            .map_err(|error| format!("resolve {}: {error}", path.display()))?,
        Ok(_) => path.to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.to_owned(),
        Err(error) => return Err(format!("inspect {}: {error}", path.display())),
    };
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;
    let lock = File::open(parent)
        .map_err(|error| format!("open directory {} for locking: {error}", parent.display()))?;
    // A busy lock is usually momentary: another Delux run, or a process this
    // one forked that still holds the inherited descriptor until it execs.
    // Wait briefly instead of failing, but never hang on a stuck holder.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => break,
            Err(rustix::io::Errno::WOULDBLOCK) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => {
                return Err(format!(
                    "cannot lock directory {} (busy or locking unsupported): {error}; nothing was changed",
                    parent.display()
                ));
            }
        }
    }
    let original = read(&target)?;
    let replacement = change(original.as_deref())?;
    if original == replacement {
        return Ok(Update {
            changed: false,
            backup: None,
        });
    }
    if replacement
        .as_ref()
        .is_some_and(|raw| raw.len() > FILE_LIMIT)
    {
        return Err(format!(
            "{} would exceed the 1 MiB agent configuration limit; nothing was written",
            path.display()
        ));
    }
    let mode = if original.is_some() {
        fs::metadata(&target)
            .map_err(|error| format!("inspect {}: {error}", target.display()))?
            .permissions()
            .mode()
    } else {
        0o600
    };
    let mut created_backup = None;
    if backup_existing && let Some(raw) = &original {
        let backup = suffixed(path, ".delux-backup");
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode & 0o7777)
            .open(&backup)
        {
            Ok(mut file) => {
                let result = file
                    .write_all(raw.as_bytes())
                    .and_then(|()| file.sync_all());
                if let Err(error) = result {
                    let _ = fs::remove_file(&backup);
                    return Err(format!("backup {}: {error}", backup.display()));
                }
                created_backup = Some(backup);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("backup {}: {error}", backup.display())),
        }
    }
    let result = match replacement {
        Some(raw) => atomic_write(&target, parent, raw.as_bytes(), mode),
        // Removing an extension symlink must remove the installed entry, not
        // leave a dangling link in the tool's extension discovery directory.
        None => {
            fs::remove_file(path).map_err(|error| format!("remove {}: {error}", path.display()))
        }
    };
    if let Err(error) = result {
        return Err(match created_backup {
            Some(backup) => format!("{error}; backup saved to {}", backup.display()),
            None => error,
        });
    }
    Ok(Update {
        changed: true,
        backup: created_backup,
    })
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn atomic_write(target: &Path, parent: &Path, raw: &[u8], mode: u32) -> Result<(), String> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let (temp, mut file) = loop {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".delux-agent-{}-{sequence}.tmp",
            std::process::id()
        ));
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
        file.write_all(raw)?;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        fs::rename(&temp, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|error| format!("write {}: {error}", target.display()))
}
