use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    #[default]
    Shell,
    Omp,
    ResumeOmp,
    Claude,
    Codex,
    Custom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InstalledLaunchers {
    pub omp: bool,
    pub claude: bool,
    pub codex: bool,
}

impl InstalledLaunchers {
    pub fn supports(&self, mode: LaunchMode) -> bool {
        match mode {
            LaunchMode::Shell | LaunchMode::Custom => true,
            LaunchMode::Omp | LaunchMode::ResumeOmp => self.omp,
            LaunchMode::Claude => self.claude,
            LaunchMode::Codex => self.codex,
        }
    }
}

pub(crate) const INSTALLED_LAUNCHERS_SCRIPT: &str = r#"printf 'delux-launchers-v1\n'
for tool in omp claude codex; do
    if command -v "$tool" >/dev/null 2>&1; then
        printf '%s=1\n' "$tool"
    else
        printf '%s=0\n' "$tool"
    fi
done
printf 'end\n'
"#;

pub(crate) fn parse_installed_launchers(output: &str) -> Result<InstalledLaunchers, String> {
    let invalid = || "Invalid installed-launcher response".to_owned();
    // Fixed order and exact delimiters reject partial output and shell startup noise.
    if output.len() != "delux-launchers-v1\nomp=0\nclaude=0\ncodex=0\nend\n".len() {
        return Err(invalid());
    }
    let mut records = output.split('\n');
    if records.next() != Some("delux-launchers-v1") {
        return Err(invalid());
    }
    let mut flag = |name: &str| -> Result<bool, String> {
        match records.next().and_then(|record| record.strip_prefix(name)) {
            Some("=0") => Ok(false),
            Some("=1") => Ok(true),
            _ => Err(invalid()),
        }
    };
    let installed = InstalledLaunchers {
        omp: flag("omp")?,
        claude: flag("claude")?,
        codex: flag("codex")?,
    };
    if records.next() != Some("end") || records.next() != Some("") || records.next().is_some() {
        return Err(invalid());
    }
    Ok(installed)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launcher {
    pub mode: LaunchMode,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub command: String,
}

impl Launcher {
    pub fn validate(&self) -> Result<(), String> {
        self.validate_profile()?;
        self.validate_command()
    }

    fn validate_profile(&self) -> Result<(), String> {
        let profile = self.profile.trim();
        if profile.is_empty() {
            return Ok(());
        }
        let base = profile.split('.').next().unwrap_or_default();
        let reserved = matches!(base, "con" | "prn" | "aux" | "nul")
            || (base.len() == 4
                && (base.starts_with("com") || base.starts_with("lpt"))
                && base.as_bytes()[3].is_ascii_digit());
        if profile.len() > 64
            || !profile.as_bytes()[0].is_ascii_alphanumeric()
            || profile.ends_with('.')
            || !profile.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
            })
            || reserved
        {
            return Err("OMP profile must start with a lowercase letter or digit, use at most 64 lowercase letters/digits/._-, and not end in '.' or be a reserved device name".into());
        }
        Ok(())
    }

    fn validate_command(&self) -> Result<(), String> {
        if self.command.chars().count() > 4096 || self.command.chars().any(char::is_control) {
            return Err("Command must be at most 4096 characters without controls".into());
        }
        if self.mode == LaunchMode::Custom && self.command.trim().is_empty() {
            return Err("Enter a custom command".into());
        }
        Ok(())
    }

    pub fn command_line(&self) -> Result<String, String> {
        match self.mode {
            LaunchMode::Shell => Ok(String::new()),
            LaunchMode::Claude => Ok("claude".to_owned()),
            LaunchMode::Codex => Ok("codex".to_owned()),
            LaunchMode::Custom => {
                self.validate_command()?;
                Ok(self.command.clone())
            }
            LaunchMode::Omp | LaunchMode::ResumeOmp => {
                self.validate_profile()?;
                let profile = self.profile.trim();
                let resume = if self.mode == LaunchMode::ResumeOmp {
                    " --resume"
                } else {
                    ""
                };
                let profile_prefix = if profile.is_empty() {
                    ""
                } else {
                    " --profile="
                };
                let mut command =
                    String::with_capacity(3 + resume.len() + profile_prefix.len() + profile.len());
                command.push_str("omp");
                command.push_str(resume);
                command.push_str(profile_prefix);
                // Validated named profiles contain only shell-literal characters.
                command.push_str(profile);
                Ok(command)
            }
        }
    }

    pub fn normalized(&self) -> Self {
        Self {
            mode: self.mode,
            profile: match self.mode {
                LaunchMode::Omp | LaunchMode::ResumeOmp => self.profile.trim().to_owned(),
                _ => String::new(),
            },
            command: match self.mode {
                LaunchMode::Custom => self.command.clone(),
                _ => String::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_use_explicit_profiles_and_native_resume_picker() {
        for (mode, profile, expected) in [
            (LaunchMode::Shell, "", ""),
            (LaunchMode::Omp, "", "omp"),
            (LaunchMode::ResumeOmp, "", "omp --resume"),
            (LaunchMode::Claude, "work", "claude"),
            (LaunchMode::Codex, "work", "codex"),
            (LaunchMode::Omp, "work.v2_1-a", "omp --profile=work.v2_1-a"),
            (LaunchMode::ResumeOmp, "work", "omp --resume --profile=work"),
            (LaunchMode::Omp, " work ", "omp --profile=work"),
            (LaunchMode::Omp, "default", "omp --profile=default"),
        ] {
            let launcher = Launcher {
                mode,
                profile: profile.into(),
                command: String::new(),
            };
            assert_eq!(launcher.command_line().unwrap(), expected);
        }
    }

    #[test]
    fn profiles_cannot_inject_shell_syntax_or_escape_named_profiles() {
        for profile in [
            ".",
            "..",
            "../work",
            "a/b",
            "a\\b",
            "~/work",
            "/work",
            "$HOME",
            "$(id)",
            "`id`",
            "a;b",
            "a|b",
            "a&b",
            "a'b",
            "a\"b",
            "a b",
            "a\nb",
            "a\rb",
            "a\tb",
            "a\0b",
            "日本語",
            "--resume",
            "_work",
            ".work",
            "Work",
            "work.",
            "con",
            "aux.txt",
            "com0",
            "lpt9.log",
        ] {
            for mode in [LaunchMode::Omp, LaunchMode::ResumeOmp] {
                let launcher = Launcher {
                    mode,
                    profile: profile.into(),
                    command: String::new(),
                };
                assert!(launcher.command_line().is_err(), "accepted {profile:?}");
            }
        }
        let mut launcher = Launcher {
            mode: LaunchMode::Omp,
            profile: "a".repeat(64),
            command: String::new(),
        };
        assert!(launcher.validate().is_ok());
        launcher.profile.push('a');
        assert!(launcher.validate().is_err());
    }

    #[test]
    fn custom_commands_are_bounded_but_preserve_intentional_shell_syntax() {
        let mut launcher = Launcher {
            mode: LaunchMode::Custom,
            profile: String::new(),
            command: "  printf '%s' \"$HOME\" && exec my-tool --flag  ".into(),
        };
        assert_eq!(launcher.command_line().unwrap(), launcher.command);
        for command in ["", "   ", "a\nb", "a\rb", "a\tb", "a\0b", "a\u{7f}b"] {
            launcher.command = command.into();
            assert!(launcher.command_line().is_err(), "accepted {command:?}");
        }
        launcher.command = "界".repeat(4096);
        assert!(launcher.validate().is_ok());
        launcher.command.push('a');
        assert!(launcher.validate().is_err());
    }

    #[test]
    fn standalone_launchers_clear_omp_and_custom_settings() {
        for mode in [LaunchMode::Claude, LaunchMode::Codex] {
            let launcher = Launcher {
                mode,
                profile: "work".into(),
                command: "echo custom".into(),
            };
            let normalized = launcher.normalized();
            assert_eq!(normalized.mode, mode);
            assert!(normalized.profile.is_empty());
            assert!(normalized.command.is_empty());
            assert_eq!(launcher.command_line(), normalized.command_line());
            assert!(normalized.validate().is_ok());
        }
    }

    #[test]
    fn discovery_checks_executable_path_without_running_launchers() {
        use crate::test_support::{TempDir, env_read};
        use std::os::unix::fs::PermissionsExt;

        let _env = env_read();
        let root = TempDir::new();
        for mask in 0..8 {
            for (index, name) in ["omp", "claude", "codex"].iter().enumerate() {
                let path = root.path().join(name);
                // Executing this fixture would fail and corrupt the protocol.
                std::fs::write(
                    &path,
                    "#!/bin/sh\nprintf 'unexpected execution\\n'\nexit 99\n",
                )
                .unwrap();
                let mode = if mask & (1 << index) != 0 {
                    0o700
                } else {
                    0o600
                };
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", INSTALLED_LAUNCHERS_SCRIPT])
                .env("PATH", root.path())
                .output()
                .unwrap();
            assert!(output.status.success());
            let installed =
                parse_installed_launchers(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
            assert_eq!(installed.omp, mask & 1 != 0);
            assert_eq!(installed.claude, mask & 2 != 0);
            assert_eq!(installed.codex, mask & 4 != 0);
            assert!(installed.supports(LaunchMode::Shell));
            assert!(installed.supports(LaunchMode::Custom));
            assert_eq!(installed.supports(LaunchMode::ResumeOmp), installed.omp);
            assert_eq!(installed.supports(LaunchMode::Omp), installed.omp);
            assert_eq!(installed.supports(LaunchMode::Claude), installed.claude);
            assert_eq!(installed.supports(LaunchMode::Codex), installed.codex);
        }
        for name in ["omp", "claude", "codex"] {
            std::fs::remove_file(root.path().join(name)).unwrap();
        }
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", INSTALLED_LAUNCHERS_SCRIPT])
            .env("PATH", root.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            parse_installed_launchers(std::str::from_utf8(&output.stdout).unwrap()).unwrap(),
            InstalledLaunchers::default()
        );
    }

    #[test]
    fn discovery_rejects_incomplete_duplicate_or_extra_records() {
        for output in [
            "",
            "delux-launchers-v1\nomp=0\nclaude=0\nend\n",
            "delux-launchers-v1\nomp=0\nomp=0\ncodex=0\nend\n",
            "delux-launchers-v1\nomp=0\nclaude=0\ncodex=0\nend\nextra\n",
            "delux-launchers-v1\nomp=0\nclaude=0\ncodex=0\nend",
            "delux-launchers-v1\nomp=0\nclaude=0\ncodex=2\nend\n",
            "delux-launchers-v1\nomp=0\nclaude=0\ncodex=0\nend\n\n",
            "noise\ndelux-launchers-v1\nomp=0\nclaude=0\ncodex=0\nend\n",
            "delux-launchers-v2\nomp=0\nclaude=0\ncodex=0\nend\n",
            "delux-launchers-v1\r\nomp=0\r\nclaude=0\r\ncodex=0\r\nend\r\n",
        ] {
            assert!(
                parse_installed_launchers(output).is_err(),
                "accepted {output:?}"
            );
        }
    }
}
