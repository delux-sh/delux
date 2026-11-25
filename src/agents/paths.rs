use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use super::{AGENTS, Integration};

pub(super) enum Directory {
    Home(&'static str),
    Parent {
        variable: &'static str,
        suffix: &'static str,
    },
    Omp,
    Config {
        variable: &'static str,
        default: &'static str,
        suffix: &'static str,
    },
}

impl Directory {
    fn resolve(
        &self,
        home: Option<&OsStr>,
        variable: &impl Fn(&str) -> Option<OsString>,
    ) -> Result<PathBuf, String> {
        let home = || {
            home.filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| "$HOME is not defined".to_owned())
        };
        match self {
            Self::Home(path) => Ok(home()?.join(path)),
            Self::Parent {
                variable: name,
                suffix,
            } => {
                let root = match variable(name).filter(|value| !value.is_empty()) {
                    Some(root) => PathBuf::from(root),
                    None => home()?,
                };
                Ok(root.join(suffix))
            }
            Self::Omp => {
                let profile =
                    omp_profile(variable("OMP_PROFILE").or_else(|| variable("PI_PROFILE")))?;
                let config = variable("PI_CONFIG_DIR")
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| ".omp".into());
                // Node path.join(home, config) retains home even for /config.
                let root = normalize_absolute(
                    &home()?.join(
                        Path::new(&config)
                            .strip_prefix("/")
                            .unwrap_or(Path::new(&config)),
                    ),
                );
                if let Some(profile) = profile {
                    return Ok(root.join("profiles").join(profile).join("agent"));
                }
                if let Some(directory) =
                    variable("PI_CODING_AGENT_DIR").filter(|value| !value.is_empty())
                {
                    let inherited = omp_profile(variable("PI_PROFILE"))
                        .ok()
                        .flatten()
                        .map(|profile| root.join("profiles").join(profile).join("agent"));
                    if inherited.as_ref().map(|path| path.as_os_str())
                        != Some(directory.as_os_str())
                    {
                        let directory = PathBuf::from(directory);
                        return if directory.is_absolute() {
                            Ok(normalize_absolute(&directory))
                        } else {
                            std::env::current_dir()
                                .map(|cwd| normalize_absolute(&cwd.join(directory)))
                                .map_err(|error| error.to_string())
                        };
                    }
                }
                Ok(root.join("agent"))
            }
            Self::Config {
                variable: name,
                default,
                suffix,
            } => {
                let root = match variable(name).filter(|value| !value.is_empty()) {
                    Some(root) => PathBuf::from(root),
                    None => home()?.join(default),
                };
                Ok(root.join(suffix))
            }
        }
    }
}

fn normalize_absolute(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn omp_profile(value: Option<OsString>) -> Result<Option<String>, String> {
    let Some(value) = value else { return Ok(None) };
    let value = value
        .to_str()
        .ok_or("Invalid OMP profile: expected UTF-8")?
        .trim();
    if value.is_empty() || value == "default" {
        return Ok(None);
    }
    let base = value.split('.').next().unwrap_or_default();
    let reserved = matches!(base, "con" | "prn" | "aux" | "nul")
        || ((base.starts_with("com") || base.starts_with("lpt"))
            && base.len() == 4
            && base.as_bytes()[3].is_ascii_digit());
    if value.len() > 64
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || b"._-".contains(&ch))
        || value.ends_with('.')
        || reserved
    {
        return Err("Invalid OMP profile: use 1–64 lowercase letters, digits, dots, underscores or hyphens; no trailing dot or reserved device name".into());
    }
    Ok(Some(value.into()))
}

pub(super) struct Context {
    pub directories: Vec<PathBuf>,
    pub path: OsString,
    pub executable: PathBuf,
    pub home: Option<PathBuf>,
    pub definitions_dir: PathBuf,
    fallback_parent_detection: Vec<bool>,
}

impl Context {
    pub fn from_inputs(
        home: Option<OsString>,
        path: OsString,
        executable: PathBuf,
        definitions_dir: PathBuf,
        variable: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, String> {
        let mut directories = Vec::with_capacity(AGENTS.len());
        let mut fallback_parent_detection = Vec::with_capacity(AGENTS.len());
        for agent in AGENTS {
            let override_path = agent
                .config_env
                .and_then(&variable)
                .filter(|value| !value.is_empty());
            let omp_override = matches!(agent.directory, Directory::Omp)
                && variable("PI_CODING_AGENT_DIR").is_some_and(|value| !value.is_empty());
            fallback_parent_detection
                .push(override_path.is_none() && !omp_override && agent.detect_fallback_parent);
            directories.push(match override_path {
                Some(path) => PathBuf::from(path),
                None => agent.directory.resolve(home.as_deref(), &variable)?,
            });
        }
        Ok(Self {
            directories,
            path,
            executable,
            home: home.map(PathBuf::from),
            definitions_dir,
            fallback_parent_detection,
        })
    }

    pub fn environment() -> Result<Self, String> {
        let config = PathBuf::from(crate::workers::default_config_path()?);
        Self::from_inputs(
            std::env::var_os("HOME"),
            std::env::var_os("PATH").unwrap_or_default(),
            std::env::current_exe()
                .map_err(|error| format!("locate running executable: {error}"))?,
            config
                .parent()
                .ok_or("default configuration has no directory")?
                .join("agents"),
            |name| std::env::var_os(name),
        )
    }

    pub fn found(&self, index: usize) -> bool {
        self.directories[index].is_dir()
            || (self.fallback_parent_detection[index]
                && self.directories[index].parent().is_some_and(Path::is_dir))
            || executable_on_path(&self.path, AGENTS[index].executable).is_some()
    }

    pub fn file(&self, index: usize) -> PathBuf {
        self.directories[index].join(match AGENTS[index].integration {
            Integration::Hooks { file, .. } | Integration::Extension { file, .. } => file,
        })
    }

    pub fn command(&self) -> Result<String, String> {
        handler_executable(
            &self.executable,
            executable_on_path(&self.path, "delux").as_deref(),
        )
    }
}

pub(super) fn executable_on_path(path: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join(name))
        .find(|candidate| {
            candidate.is_file()
                && rustix::fs::access(candidate, rustix::fs::Access::EXEC_OK).is_ok()
        })
}

pub(super) fn handler_executable(
    executable: &Path,
    on_path: Option<&Path>,
) -> Result<String, String> {
    if let Some(on_path) = on_path
        && let (Ok(current), Ok(found)) = (executable.canonicalize(), on_path.canonicalize())
        && current == found
    {
        return Ok("delux".into());
    }
    if !executable.is_absolute() {
        return Err("running executable path is not absolute".into());
    }
    let executable = executable
        .to_str()
        .ok_or("running executable path is not valid UTF-8")?;
    Ok(format!("'{}'", executable.replace('\'', "'\\''")))
}

// Recognize only the executable spellings emitted by handler_executable, not
// arbitrary shell commands that happen to mention our reporter arguments.
pub(super) fn is_handler_executable(command: &str) -> bool {
    command == "delux"
        || command
            .strip_prefix('\'')
            .and_then(|command| command.strip_suffix('\''))
            .is_some_and(|path| {
                path.starts_with('/') && path.split("'\\''").all(|part| !part.contains('\''))
            })
}
