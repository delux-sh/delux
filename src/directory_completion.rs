use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::tmux::shell_quote;

const MAX_MATCHES: usize = 100;
const MAX_REPLY_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryCompletion {
    pub path: String,
    pub exists: bool,
    pub matches: Vec<String>,
    pub truncated: bool,
}

fn normalize(path: &str) -> Result<&str, String> {
    if path.chars().count() > 4096 || path.chars().any(char::is_control) {
        return Err("Directory must be at most 4096 characters without controls".into());
    }
    match path {
        "" | "~" => Ok("~/"),
        path if path.starts_with("~/") || path.starts_with('/') => Ok(path),
        _ => Err("Directory must be an absolute path, ~, or ~/path".into()),
    }
}

/// Read-only POSIX shell lookup. Only our final wildcard is expanded: every
/// user-supplied path fragment stays quoted, including glob metacharacters.
pub fn script(path: &str) -> Result<String, String> {
    let path = normalize(path)?;
    let mut script = String::from("path=");
    script.push_str(&shell_quote(path));
    script.push_str(LOOKUP_SCRIPT);
    Ok(script)
}

// Versioned, newline-delimited protocol: header, existence bit, up to 100
// base64-encoded full paths, and a mandatory footer carrying the truncation bit.
// The transport also bounds output and runtime; the script stays below 256 KiB.
const LOOKUP_SCRIPT: &str = r#"
LC_ALL=C
export LC_ALL
command -v base64 >/dev/null 2>&1 || exit 72
command -v tr >/dev/null 2>&1 || exit 72
encode() {
    encoded=$(printf '%s' "$1" | base64) || exit 72
    printf '%s' "$encoded" | tr -d '\r\n' || exit 72
}
probe=$(encode 'delux') || exit 72
[ "$probe" = 'ZGVsdXg=' ] || exit 72
missing() {
    printf '%s\n' 'DELUX_DIRECTORY_1' 'E 0' 'END 0'
    exit 0
}
inaccessible() {
    printf '%s\n' 'The directory parent is not readable or searchable.' >&2
    exit 1
}
directory=$path
case "$directory" in
    '~/'*) directory=$HOME/${directory#??} ;;
esac
case "$directory" in
    /*) ;;
    *) printf '%s\n' 'The target computer HOME is not an absolute directory.' >&2; exit 1 ;;
esac
parent=${directory%/*}
[ -n "$parent" ] || parent=/
prefix=${directory##*/}
display_parent=${path%/*}/
# Check each ancestor before treating a failed -d as absence. Search permission
# is sufficient for ancestors; the listed parent also needs read permission.
remaining=${parent#/}
current=
[ -x / ] || inaccessible
while [ -n "$remaining" ]; do
    component=${remaining%%/*}
    case "$remaining" in
        */*) remaining=${remaining#*/} ;;
        *) remaining= ;;
    esac
    [ -n "$component" ] || continue
    current=$current/$component
    [ -d "$current" ] || missing
    [ -x "$current" ] || inaccessible
done
[ -r "$parent" ] && [ -x "$parent" ] || inaccessible
exists=0
[ ! -d "$directory" ] || exists=1
printf '%s\n' 'DELUX_DIRECTORY_1' "E $exists"
count=0
bytes=0
truncated=0
for candidate in "$parent"/"$prefix"*; do
    [ -d "$candidate" ] || continue
    name=${candidate##*/}
    case "$name" in '.'|'..') continue ;; esac
    if [ "$count" -ge 100 ]; then
        truncated=1
        break
    fi
    match=$display_parent$name/
    # A valid form path is at most 4096 Unicode characters (16384 UTF-8 bytes).
    if [ "${#match}" -gt 16384 ]; then
        printf '%s\n' 'A directory completion exceeds the supported path length.' >&2
        exit 1
    fi
    encoded=$(encode "$match") || exit 72
    bytes=$((bytes + ${#encoded} + 3))
    if [ "$bytes" -gt 240000 ]; then
        truncated=1
        break
    fi
    printf 'M %s\n' "$encoded"
    count=$((count + 1))
done
printf 'END %s\n' "$truncated"
"#;

pub fn parse(path: &str, output: &str) -> Result<DirectoryCompletion, String> {
    let path = normalize(path)?;
    let invalid = || "Malformed or unsafe directory completion result".to_owned();
    if output.len() > MAX_REPLY_BYTES || !output.ends_with('\n') {
        return Err(invalid());
    }
    let mut lines = output.split_terminator('\n');
    if lines.next() != Some("DELUX_DIRECTORY_1") {
        return Err(invalid());
    }
    let exists = match lines.next() {
        Some("E 0") => false,
        Some("E 1") => true,
        _ => return Err(invalid()),
    };
    let mut matches = Vec::new();
    let truncated = loop {
        match lines.next() {
            Some("END 0") => break false,
            Some("END 1") => break true,
            Some(line) => {
                if matches.len() == MAX_MATCHES {
                    return Err(invalid());
                }
                let encoded = line.strip_prefix("M ").ok_or_else(invalid)?;
                let decoded = STANDARD.decode(encoded).map_err(|_| invalid())?;
                let matched = String::from_utf8(decoded).map_err(|_| invalid())?;
                // Reject controls, unrelated paths, nested descendants, and
                // shell-generated dot/parent entries rather than displaying them.
                normalize(&matched).map_err(|_| invalid())?;
                let suffix = matched.strip_prefix(path).ok_or_else(invalid)?;
                let suffix = suffix.strip_suffix('/').ok_or_else(invalid)?;
                if suffix.contains('/') {
                    return Err(invalid());
                }
                let name = matched[..matched.len() - 1]
                    .rsplit('/')
                    .next()
                    .ok_or_else(invalid)?;
                if name.is_empty()
                    || name == "."
                    || name == ".."
                    || (name.starts_with('.') && !path.rsplit('/').next().unwrap().starts_with('.'))
                {
                    return Err(invalid());
                }
                matches.push(matched);
            }
            None => return Err(invalid()),
        }
    };
    if lines.next().is_some() {
        return Err(invalid());
    }
    matches.sort_unstable();
    if matches.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid());
    }
    Ok(DirectoryCompletion {
        path: path.to_owned(),
        exists,
        matches,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::{Command, Output};

    use super::*;
    use crate::test_support::{TempDir, env_read};

    fn run(home: &TempDir, path: &str) -> Output {
        Command::new("/bin/sh")
            .args(["-c", &script(path).unwrap()])
            .env("HOME", home.path())
            .current_dir(home.path())
            .output()
            .unwrap()
    }

    fn lookup(home: &TempDir, path: &str) -> DirectoryCompletion {
        let output = run(home, path);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        parse(path, &String::from_utf8(output.stdout).unwrap()).unwrap()
    }

    #[test]
    fn normalizes_home_and_preserves_unicode_spaces_and_absolute_paths() {
        let _environment = env_read();
        let home = TempDir::new();
        fs::create_dir(home.path().join("日本語 projects")).unwrap();
        for input in ["", "~", "~/"] {
            let result = lookup(&home, input);
            assert_eq!(result.path, "~/");
            assert!(result.exists);
            assert_eq!(result.matches, ["~/日本語 projects/"]);
            assert!(!result.truncated);
        }
        let result = lookup(&home, "~/日");
        assert!(!result.exists);
        assert_eq!(result.matches, ["~/日本語 projects/"]);
        let absolute = format!("{}/日本語 projects", home.path().display());
        let result = lookup(&home, &absolute);
        assert!(result.exists);
        assert_eq!(result.matches, [format!("{absolute}/")]);
    }

    #[test]
    fn treats_shell_injection_and_globs_as_literal_names() {
        let _environment = env_read();
        let home = TempDir::new();
        let names = [
            "$(touch INJECTED)",
            "`touch INJECTED`",
            "'; touch INJECTED; '",
            "$HOME",
            "star*literal",
            "question?literal",
            "bracket[ab]literal",
            "quote'\"literal;#%(x)",
        ];
        for name in names {
            fs::create_dir(home.path().join(name)).unwrap();
            fs::create_dir(home.path().join(name).join("child")).unwrap();
            let query = format!("~/{name}");
            let result = lookup(&home, &query);
            assert!(result.exists);
            assert_eq!(result.matches, [format!("{query}/")]);
            assert_eq!(
                lookup(&home, &format!("{query}/")).matches,
                [format!("{query}/child/")]
            );
        }
        assert!(!home.path().join("INJECTED").exists());
        assert!(lookup(&home, "~/star?").matches.is_empty());
        assert!(lookup(&home, "~/bracket*").matches.is_empty());
    }

    #[test]
    fn filters_files_hidden_entries_and_broken_links_but_follows_directory_links() {
        let _environment = env_read();
        let home = TempDir::new();
        fs::create_dir(home.path().join("visible")).unwrap();
        fs::create_dir(home.path().join(".hidden")).unwrap();
        fs::write(home.path().join("file"), "data").unwrap();
        symlink("visible", home.path().join("linked")).unwrap();
        symlink("absent", home.path().join("broken")).unwrap();
        symlink("file", home.path().join("file-link")).unwrap();
        assert_eq!(lookup(&home, "~/").matches, ["~/linked/", "~/visible/"]);
        assert_eq!(lookup(&home, "~/.").matches, ["~/.hidden/"]);
        assert!(lookup(&home, "~/linked").exists);
        assert!(!lookup(&home, "~/file").exists);
        assert!(lookup(&home, "~/file").matches.is_empty());
    }

    #[test]
    fn missing_or_non_directory_parents_are_absence_not_lookup_errors() {
        let _environment = env_read();
        let home = TempDir::new();
        fs::write(home.path().join("file"), "data").unwrap();
        for path in ["~/missing/child", "~/missing/", "~/file/child"] {
            let result = lookup(&home, path);
            assert!(!result.exists);
            assert!(result.matches.is_empty());
            assert!(!result.truncated);
        }
    }

    #[test]
    fn inaccessible_parents_and_ancestors_are_errors() {
        let _environment = env_read();
        let home = TempDir::new();
        let blocked = home.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::create_dir(blocked.join("child")).unwrap();
        for mode in [0o000, 0o100] {
            fs::set_permissions(&blocked, fs::Permissions::from_mode(mode)).unwrap();
            // Root can bypass ordinary mode bits; only assert denial when the
            // actual shell cannot read and search this fixture.
            let denied = !Command::new("/bin/sh")
                .args(["-c", "test -r \"$1\" && test -x \"$1\"", "sh"])
                .arg(&blocked)
                .status()
                .unwrap()
                .success();
            let output = run(&home, "~/blocked/");
            let ancestor = (mode == 0o000).then(|| run(&home, "~/blocked/child/missing/"));
            fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
            if denied {
                for output in std::iter::once(output).chain(ancestor) {
                    assert!(!output.status.success());
                    assert!(
                        String::from_utf8(output.stderr)
                            .unwrap()
                            .contains("not readable or searchable")
                    );
                }
            }
        }
    }

    #[test]
    fn caps_results_and_reports_truncation_only_when_more_matches_exist() {
        let _environment = env_read();
        let home = TempDir::new();
        for index in 0..100 {
            fs::create_dir(home.path().join(format!("entry-{index:03}"))).unwrap();
        }
        let complete = lookup(&home, "~/");
        assert_eq!(complete.matches.len(), MAX_MATCHES);
        assert!(!complete.truncated);
        fs::create_dir(home.path().join("entry-100")).unwrap();
        let capped = lookup(&home, "~/");
        assert_eq!(capped.matches, complete.matches);
        assert!(capped.truncated);
        assert_eq!(lookup(&home, "~/entry-100").matches, ["~/entry-100/"]);
    }

    #[test]
    fn rejects_unsafe_names_from_a_real_shell() {
        let _environment = env_read();
        let home = TempDir::new();
        fs::create_dir(home.path().join("unsafe\nname")).unwrap();
        let output = run(&home, "~/");
        assert!(output.status.success());
        assert!(parse("~/", &String::from_utf8(output.stdout).unwrap()).is_err());
    }

    #[test]
    fn rejects_invalid_input_and_malformed_protocol() {
        for path in ["relative", "~other", "/bad\npath", "/bad\0path"] {
            assert!(script(path).is_err());
        }
        assert!(script(&format!("/{}", "a".repeat(4096))).is_err());
        let invalid_utf8 = format!(
            "DELUX_DIRECTORY_1\nE 1\nM {}\nEND 0\n",
            STANDARD.encode(b"~/invalid\xff/")
        );
        assert!(parse("~/", &invalid_utf8).is_err());
        for output in [
            "",
            "DELUX_DIRECTORY_1\nE 1\n",
            "DELUX_DIRECTORY_1\nE 2\nEND 0\n",
            "DELUX_DIRECTORY_1\nE 1\nEND 0",
            "DELUX_DIRECTORY_1\nE 1\nEND 0\nextra\n",
            "DELUX_DIRECTORY_1\nE 1\nM !!!\nEND 0\n",
        ] {
            assert!(parse("~/", output).is_err());
        }
        for matched in ["/other/", "~/nested/child/", "~/../", "~/.//", "~/bad\r/"] {
            let output = format!(
                "DELUX_DIRECTORY_1\nE 1\nM {}\nEND 0\n",
                STANDARD.encode(matched)
            );
            assert!(parse("~/", &output).is_err(), "{matched:?}");
        }
        let line = format!("M {}\n", STANDARD.encode("~/same/"));
        for count in [2, MAX_MATCHES + 1] {
            let output = format!("DELUX_DIRECTORY_1\nE 1\n{}END 0\n", line.repeat(count));
            assert!(parse("~/", &output).is_err());
        }
        assert!(parse("~/", &"x".repeat(MAX_REPLY_BYTES + 1)).is_err());
    }
}
