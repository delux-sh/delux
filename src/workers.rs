use crate::cancel::Cancel;
use crate::files::{read_utf8, xdg_path};
use crate::options::Options;
use crate::process::run_command_with_env;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

const DISCOVERY_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
const INVALID_CONFIG_SHAPE: &str =
    "Use a hosts object of name: SSH-destination pairs and an ignore array of names";
const INVALID_INVENTORY: &str = "Tailscale returned a malformed peer inventory";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub hosts: BTreeMap<String, String>,
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Worker {
    pub key: String,
    pub name: String,
    pub destination: Option<String>,
    pub online: Option<bool>,
}

pub fn validate_destination(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 1024
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.@:%+[]-".contains(&c))
    {
        return Err("SSH destination must be a host/alias or user@host, not a command".into());
    }
    Ok(())
}

/// Replaces or adds an SSH user without changing the host or IPv6 brackets.
pub fn with_user(destination: &str, user: &str) -> Result<String, String> {
    if user.is_empty()
        || user.len() > 64
        || user.starts_with('-')
        || !user
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return Err(
            "SSH user must be 1–64 ASCII letters, digits, '.', '_', or '-', and not start with '-'"
                .into(),
        );
    }
    validate_destination(destination)?;
    let host = destination
        .rsplit_once('@')
        .map_or(destination, |(_, host)| host);
    let result = format!("{user}@{host}");
    validate_destination(&result)?;
    Ok(result)
}

pub fn default_config_path() -> Result<String, String> {
    xdg_path("XDG_CONFIG_HOME", ".config", "cli.json")
}

pub fn load_config(path: &str) -> Result<Config, String> {
    let Some(raw) = read_utf8(path, "CLI configuration")? else {
        return Ok(Config::default());
    };
    let fields: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|_| "CLI configuration must be a JSON object".to_string())?;
    let fields = match fields {
        serde_json::Value::Object(fields) => fields,
        _ => return Err("CLI configuration must be a JSON object".into()),
    };
    if fields
        .keys()
        .any(|name| name != "hosts" && name != "ignore")
    {
        return Err("CLI configuration accepts only 'hosts' and 'ignore'".into());
    }
    let mut config = Config::default();
    if let Some(hosts) = fields.get("hosts") {
        let hosts = hosts.as_object().ok_or(INVALID_CONFIG_SHAPE)?;
        for (name, target) in hosts {
            let target = target.as_str().ok_or(INVALID_CONFIG_SHAPE)?;
            config.hosts.insert(name.clone(), target.into());
        }
    }
    if let Some(ignored) = fields.get("ignore") {
        let ignored = ignored.as_array().ok_or(INVALID_CONFIG_SHAPE)?;
        for name in ignored {
            let name = name.as_str().ok_or(INVALID_CONFIG_SHAPE)?;
            config.ignore.push(name.into());
        }
    }
    for (name, target) in &config.hosts {
        if name.is_empty() {
            return Err(INVALID_CONFIG_SHAPE.into());
        }
        validate_destination(target)?;
    }
    if config.ignore.iter().any(String::is_empty) {
        return Err(INVALID_CONFIG_SHAPE.into());
    }
    Ok(config)
}

/// Atomically saves a host mapping under the configuration's persistent lock, merging
/// with the current file so concurrent savers keep each other's hosts and ignored names.
/// Refuses invalid existing configuration and uses private directory/file modes.
pub fn save_host_mapping(path: &str, name: &str, destination: &str) -> Result<(), String> {
    #[derive(Serialize)]
    struct SavedConfig<'a> {
        hosts: &'a BTreeMap<String, String>,
        ignore: &'a [String],
    }
    if name.is_empty() {
        return Err("Host mapping name must not be empty".into());
    }
    validate_destination(destination)?;
    crate::state::locked_update(path, |target| {
        let mut config = load_config(target)?;
        config.hosts.insert(name.into(), destination.into());
        let mut raw = serde_json::to_vec_pretty(&SavedConfig {
            hosts: &config.hosts,
            ignore: &config.ignore,
        })
        .map_err(|error| format!("Serialize CLI configuration: {error}"))?;
        raw.push(b'\n');
        Ok(((), raw))
    })
}

pub fn local_worker() -> Worker {
    Worker {
        key: "local".into(),
        name: "local".into(),
        destination: None,
        online: Some(true),
    }
}

pub fn explicit_worker(name: &str, config: &Config) -> Result<Worker, String> {
    if name == "local" {
        return Ok(local_worker());
    }
    let target = config.hosts.get(name).map(String::as_str).unwrap_or(name);
    validate_destination(target)?;
    Ok(Worker {
        key: name.into(),
        name: name.into(),
        destination: Some(target.into()),
        online: None,
    })
}

pub fn tailscale_command() -> Option<String> {
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("tailscale");
            if !candidate.is_file() {
                continue;
            }
            if rustix::fs::access(&candidate, rustix::fs::Access::EXEC_OK).is_err() {
                continue;
            }
            // Never execute a program found relative to the working directory.
            if candidate.is_absolute() {
                return Some(candidate.to_string_lossy().into_owned());
            }
            break;
        }
    }
    #[cfg(target_os = "macos")]
    {
        let bundled = std::path::Path::new("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        if bundled.is_file() {
            return Some(bundled.to_string_lossy().into_owned());
        }
    }
    None
}

fn field<'a, T: Deserialize<'a>>(
    fields: &'a serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<Option<T>, String> {
    fields
        .get(name)
        .map(T::deserialize)
        .transpose()
        .map_err(|_| INVALID_INVENTORY.into())
}

fn tagged(fields: &serde_json::Map<String, serde_json::Value>) -> Result<bool, String> {
    match fields.get("Tags") {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(value) => {
            let tags = value.as_array().ok_or(INVALID_INVENTORY)?;
            if tags.iter().any(|tag| !tag.is_string()) {
                return Err(INVALID_INVENTORY.into());
            }
            Ok(!tags.is_empty())
        }
    }
}

fn ignored_worker(name: &str, config: &Config) -> bool {
    config.ignore.iter().any(|ignored| name == ignored)
}

pub fn configured_workers(config: &Config, matched: &HashSet<String>) -> Vec<Worker> {
    config
        .hosts
        .iter()
        .filter(|(name, _)| {
            name.as_str() != "local" && !matched.contains(*name) && !ignored_worker(name, config)
        })
        .map(|(name, target)| Worker {
            key: name.clone(),
            name: name.clone(),
            destination: Some(target.clone()),
            online: None,
        })
        .collect()
}

pub fn peer_workers(raw: &str, config: &Config) -> Result<Vec<Worker>, String> {
    let status: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(raw).map_err(|_| INVALID_INVENTORY.to_string())?;
    let (own_user, own_tagged) = match status.get("Self") {
        Some(value) => {
            let own = value.as_object().ok_or(INVALID_INVENTORY)?;
            (field::<u64>(own, "UserID")?, tagged(own)?)
        }
        None => (None, false),
    };
    let peers = status
        .get("Peer")
        .map(|value| value.as_object().ok_or(INVALID_INVENTORY))
        .transpose()?;
    if field::<&str>(&status, "BackendState")? != Some("Running") {
        return Err(
            "Tailscale is not running and connected; use --host for explicit SSH computers".into(),
        );
    }
    let mut workers =
        Vec::with_capacity(peers.map_or(0, serde_json::Map::len) + config.hosts.len());
    let mut matched = HashSet::new();
    for peer in peers.into_iter().flat_map(serde_json::Map::values) {
        let peer = peer.as_object().ok_or(INVALID_INVENTORY)?;
        let id = field::<&str>(peer, "ID")?.unwrap_or("");
        let dns = field::<&str>(peer, "DNSName")?
            .unwrap_or("")
            .trim_end_matches('.');
        let host_name = field::<&str>(peer, "HostName")?.unwrap_or("");
        let os = field::<&str>(peer, "OS")?.unwrap_or("");
        let online = field::<bool>(peer, "Online")?;
        let shared = field::<bool>(peer, "ShareeNode")?.unwrap_or(false);
        let user = field::<u64>(peer, "UserID")?;
        let peer_tagged = tagged(peer)?;
        let restricted = shared
            || own_tagged
            || peer_tagged
            || !matches!((own_user, user), (Some(own), Some(peer)) if own != 0 && own == peer);
        let addresses = peer
            .get("TailscaleIPs")
            .map(|value| value.as_array().ok_or(INVALID_INVENTORY))
            .transpose()?;
        if addresses.is_some_and(|values| values.iter().any(|value| !value.is_string())) {
            return Err(INVALID_INVENTORY.into());
        }
        let short = dns.split_once('.').map(|(short, _)| short).unwrap_or(dns);
        let aliases = [id, dns, short, host_name];
        let mut mapping = "";
        let mut ignored = false;
        // Restricted peers cannot claim local aliases through their names.
        let aliases = if restricted {
            &aliases[..2]
        } else {
            &aliases[..]
        };
        for &alias in aliases {
            if alias.is_empty() {
                continue;
            }
            if let Some(target) = config.hosts.get(alias) {
                matched.insert(alias.to_owned());
                if mapping.is_empty() {
                    mapping = target;
                }
            }
            if ignored_worker(alias, config) {
                ignored = true;
            }
        }
        if ignored || (mapping.is_empty() && (restricted || (os != "linux" && os != "macOS"))) {
            continue;
        }
        let mut target = if mapping.is_empty() { dns } else { mapping };
        if target.is_empty() {
            target = addresses
                .and_then(|values| values.first())
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
        }
        if target.is_empty() {
            continue;
        }
        validate_destination(target)?;
        let identity = if !id.is_empty() {
            id
        } else if !dns.is_empty() {
            dns
        } else {
            target
        };
        let name = if !short.is_empty() {
            short
        } else if !host_name.is_empty() {
            host_name
        } else {
            target
        };
        workers.push(Worker {
            key: identity.into(),
            name: name.into(),
            destination: Some(target.into()),
            online,
        });
    }
    workers.extend(configured_workers(config, &matched));
    Ok(workers)
}

pub fn discover(
    cancel: &Cancel,
    config: &Config,
    opts: &Options,
) -> Result<(Vec<Worker>, String), String> {
    let mut workers = Vec::with_capacity(1 + opts.hosts.len() + config.hosts.len());
    if !opts.no_local {
        workers.push(local_worker());
    }
    if opts.local_only {
        return Ok((workers, String::new()));
    }
    let mut warning = String::new();
    if !opts.hosts.is_empty() {
        for name in &opts.hosts {
            workers.push(explicit_worker(name, config)?);
        }
    } else {
        let found = match tailscale_command() {
            None => Err(
                "Tailscale CLI not found; using local/configured computers. Use --host to add one"
                    .into(),
            ),
            Some(command) => {
                match run_command_with_env(
                    cancel,
                    &[command, "status".into(), "--json".into()],
                    Duration::from_secs(5),
                    false,
                    &[("TAILSCALE_BE_CLI", "1")],
                    DISCOVERY_OUTPUT_LIMIT,
                ) {
                    Err(error) => Err(error.to_string()),
                    Ok(result) if result.code != 0 => {
                        let detail = result.stderr.trim();
                        Err(if detail.is_empty() {
                            format!("Tailscale discovery failed (exit status {})", result.code)
                        } else {
                            detail.into()
                        })
                    }
                    Ok(result) => peer_workers(&result.stdout, config),
                }
            }
        };
        match found {
            Ok(found) => workers.extend(found),
            Err(error) => {
                warning = error;
                workers.extend(configured_workers(config, &HashSet::new()));
            }
        }
    }
    // Preserve discovery order while retaining the latest metadata per destination.
    let mut positions = HashMap::with_capacity(workers.len());
    let mut unique = Vec::with_capacity(workers.len());
    for worker in workers {
        let target = worker.destination.as_deref().unwrap_or("");
        if let Some(&index) = positions.get(target) {
            unique[index] = worker;
        } else {
            positions.insert(target.to_owned(), unique.len());
            unique.push(worker);
        }
    }
    Ok((unique, warning))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EnvVar, TempDir, env_write};

    fn config(hosts: &[(&str, &str)], ignore: &[&str]) -> Config {
        Config {
            hosts: hosts
                .iter()
                .map(|(name, target)| ((*name).into(), (*target).into()))
                .collect(),
            ignore: ignore.iter().map(|name| (*name).into()).collect(),
        }
    }

    #[test]
    fn destinations_reject_commands_and_accept_ssh_addresses() {
        for value in [
            "-oProxyCommand=id",
            "host\nname",
            "user@host;id",
            "$(id)",
            "host name",
            "",
            "é",
            &"a".repeat(1025),
        ] {
            assert!(
                validate_destination(value).is_err(),
                "accepted invalid SSH destination: {value:?}"
            );
        }
        for value in [
            "my-alias",
            "alice@host",
            "100.64.0.1",
            "user@fd7a:115c:a1e0::1",
            "user@[fe80::1%en0]",
            "name+suffix",
            &"a".repeat(1024),
        ] {
            assert!(validate_destination(value).is_ok());
        }
    }

    #[test]
    fn with_user_adds_replaces_and_preserves_ipv6() {
        for (target, user, expected) in [
            ("host", "alice", "alice@host"),
            ("bob@host", "alice", "alice@host"),
            ("[fe80::1%en0]", "alice", "alice@[fe80::1%en0]"),
            ("bob@[fe80::1%en0]", "alice", "alice@[fe80::1%en0]"),
            ("bob@fd7a:115c:a1e0::1", "alice", "alice@fd7a:115c:a1e0::1"),
            ("my-alias", "A.z_0-9", "A.z_0-9@my-alias"),
        ] {
            let result = with_user(target, user).unwrap();
            assert_eq!(result, expected);
            assert!(validate_destination(&result).is_ok());
        }
        assert!(with_user("host", &"a".repeat(64)).is_ok());
        for user in [
            "", "-alice", "a b", "a@b", "a:b", "a+b", "a/b", "é", "a\n", "a\0",
        ] {
            assert!(with_user("host", user).is_err(), "accepted user {user:?}");
        }
        assert!(with_user("host", &"a".repeat(65)).is_err());
        for target in ["", "-host", "host;id", "host name"] {
            assert!(with_user(target, "alice").is_err());
        }
        assert!(with_user(&"a".repeat(1024), "alice").is_err());
    }

    #[test]
    fn host_mapping_creates_private_config_and_preserves_existing_values() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let parent = dir.path().join("nested/delux");
        let path = parent.join("cli.json");
        let path = path.to_str().unwrap();
        save_host_mapping(path, "dev", "alice@[fe80::1%en0]").unwrap();
        assert_eq!(
            load_config(path).unwrap(),
            config(&[("dev", "alice@[fe80::1%en0]")], &[])
        );
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(std::fs::read(path).unwrap().ends_with(b"\n"));
        std::fs::write(
            path,
            r#"{"hosts":{"dev":"old@dev","other":"bob@other"},"ignore":["phone","tablet"]}"#,
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        save_host_mapping(path, "dev", "new@dev").unwrap();
        save_host_mapping(path, "third", "third-alias").unwrap();
        assert_eq!(
            load_config(path).unwrap(),
            config(
                &[
                    ("dev", "new@dev"),
                    ("other", "bob@other"),
                    ("third", "third-alias")
                ],
                &["phone", "tablet"]
            )
        );
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn host_mapping_refuses_invalid_existing_config_and_invalid_mapping() {
        let dir = TempDir::new();
        let path = dir.path().join("cli.json");
        let path = path.to_str().unwrap();
        for raw in [
            "{",
            r#"{"unknown":true}"#,
            r#"{"hosts":{"bad":"-invalid"}}"#,
            r#"{"ignore":[""]}"#,
        ] {
            std::fs::write(path, raw).unwrap();
            assert!(save_host_mapping(path, "dev", "alice@dev").is_err());
            assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
        }
        let raw = r#"{"hosts":{"dev":"alice@dev"},"ignore":[]}"#;
        std::fs::write(path, raw).unwrap();
        for (name, target) in [
            ("", "host"),
            ("dev", ""),
            ("dev", "-host"),
            ("dev", "host;id"),
        ] {
            assert!(save_host_mapping(path, name, target).is_err());
            assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
        }
    }

    #[test]
    fn concurrent_host_mappings_merge_under_the_configuration_lock() {
        use std::os::fd::AsRawFd;

        let dir = TempDir::new();
        let path = dir.path().join("cli.json");
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.path().join("cli.json.lock"))
            .unwrap();
        // SAFETY: lock owns this descriptor for the duration of the call.
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
        let savers: Vec<_> = [("a", "alice@a"), ("b", "bob@b")]
            .into_iter()
            .map(|(name, destination)| {
                let path = path.clone();
                std::thread::spawn(move || {
                    save_host_mapping(path.to_str().unwrap(), name, destination)
                })
            })
            .collect();
        // Another writer holding the lock: savers must merge with its result, not replace it.
        std::fs::write(&path, r#"{"hosts":{"c":"carol@c"},"ignore":["phone"]}"#).unwrap();
        drop(lock);
        for saver in savers {
            saver.join().unwrap().unwrap();
        }
        assert_eq!(
            load_config(path.to_str().unwrap()).unwrap(),
            config(
                &[("a", "alice@a"), ("b", "bob@b"), ("c", "carol@c")],
                &["phone"]
            )
        );
    }

    #[test]
    fn default_config_path_uses_xdg_on_every_platform() {
        let _env = env_write();
        let home = TempDir::new();
        let xdg = home.path().join("xdg");
        let _home = EnvVar::set("HOME", home.path());
        let _xdg = EnvVar::set("XDG_CONFIG_HOME", &xdg);
        assert_eq!(
            default_config_path().unwrap(),
            xdg.join("delux/cli.json").to_str().unwrap()
        );
        let unnormalized = home.path().join("linked/../xdg");
        let _unnormalized_xdg = EnvVar::set("XDG_CONFIG_HOME", &unnormalized);
        assert_eq!(
            default_config_path().unwrap(),
            unnormalized.join("delux/cli.json").to_str().unwrap()
        );
        let _empty_xdg = EnvVar::set("XDG_CONFIG_HOME", "");
        assert_eq!(
            default_config_path().unwrap(),
            home.path().join(".config/delux/cli.json").to_str().unwrap()
        );
    }

    #[test]
    fn configuration_rejects_invalid_shapes_and_accepts_bounded_utf8() {
        let dir = TempDir::new();
        let path = dir.path().join("cli.json");
        let path = path.to_str().unwrap();
        assert_eq!(load_config(path).unwrap(), Config::default());
        let mut invalid: Vec<Vec<u8>> = [
            "",
            "null",
            "[]",
            "42",
            "{} {}",
            r#"{"host":{"dev":"alice@dev"}}"#,
            r#"{"hosts":[]}"#,
            r#"{"hosts":null}"#,
            r#"{"hosts":{"dev":"-oProxyCommand=id"}}"#,
            r#"{"hosts":{"dev":7}}"#,
            r#"{"hosts":{"dev":null}}"#,
            r#"{"hosts":{"":"valid-host"}}"#,
            r#"{"ignore":"dev"}"#,
            r#"{"ignore":null}"#,
            r#"{"ignore":[42]}"#,
            r#"{"ignore":[null]}"#,
            r#"{"ignore":[""]}"#,
        ]
        .iter()
        .map(|raw| raw.as_bytes().to_vec())
        .collect();
        invalid.push(b"{\"hosts\":{\"\xff\":\"valid-host\"}}".to_vec());
        invalid.push(format!("{}{{}}", " ".repeat(64 * 1024)).into_bytes());
        for (index, raw) in invalid.iter().enumerate() {
            std::fs::write(path, raw).unwrap();
            assert!(
                load_config(path).is_err(),
                "accepted invalid configuration case {index}"
            );
        }
        std::fs::write(path, r#"{"hosts":{"node-1":"alice@dev","Dev Laptop":"my-alias"},"ignore":["phone","old.tail.test"]}"#).unwrap();
        assert_eq!(
            load_config(path).unwrap(),
            config(
                &[("node-1", "alice@dev"), ("Dev Laptop", "my-alias")],
                &["phone", "old.tail.test"]
            )
        );
        std::fs::write(path, format!("{{}}{}", " ".repeat(64 * 1024 - 2))).unwrap();
        assert!(load_config(path).is_ok(), "rejected exactly 64 KiB");
    }

    #[test]
    fn peer_mapping_precedence_and_offline_hint() {
        let status = r#"{"BackendState":"Running","Version":"1.90.0","Self":{"ID":"self","UserID":10},"Peer":{
            "nodekey:a":{"ID":"node-1","DNSName":"dev.tail.test.","HostName":"Dev Laptop","OS":"macOS","UserID":10,"Online":false,"TailscaleIPs":["100.64.0.1","fd7a:115c:a1e0::1"],"LastSeen":"2025-01-01T00:00:00Z"},
            "nodekey:b":{"ID":"node-2","DNSName":"phone.tail.test.","OS":"iOS","UserID":10},
            "nodekey:c":{"ID":"node-3","DNSName":"ignore.tail.test.","OS":"linux","UserID":10}
        }}"#;
        let aliases = ["node-1", "dev.tail.test", "dev", "Dev Laptop"];
        for first in 0..aliases.len() {
            let mut config = config(&[("extra", "bob@extra")], &["ignore"]);
            for alias in &aliases[first..] {
                config
                    .hosts
                    .insert((*alias).into(), "lower-priority".into());
            }
            config
                .hosts
                .insert(aliases[first].into(), "alice@dev-alias".into());
            let workers = peer_workers(status, &config).unwrap();
            assert_eq!(workers.len(), 2);
            assert_eq!(
                workers[0],
                Worker {
                    key: "node-1".into(),
                    name: "dev".into(),
                    destination: Some("alice@dev-alias".into()),
                    online: Some(false)
                }
            );
            assert_eq!(workers[1].key, "extra");
            assert_eq!(workers[1].destination.as_deref(), Some("bob@extra"));
            assert_eq!(workers[1].online, None);
        }
    }

    #[test]
    fn peer_filtering_and_explicit_opt_in() {
        let status = r#"{"BackendState":"Running","Self":{"UserID":10},"Peer":{
            "b":{"ID":"bsd","DNSName":"bsd.tail.test.","OS":"freebsd","UserID":10},
            "a":{"ID":"linux","DNSName":"linux.tail.test.","OS":"linux","UserID":10},
            "c":{"ID":"phone","DNSName":"phone.tail.test.","OS":"iOS","UserID":10},
            "d":{"ID":"mac","DNSName":"mac.tail.test.","OS":"macOS","UserID":10}
        }}"#;
        let workers = peer_workers(status, &Config::default()).unwrap();
        assert_eq!(workers.len(), 2);
        assert_eq!(workers[0].key, "linux");
        assert_eq!(workers[1].key, "mac");
        let config = config(&[("bsd", "dev@bsd")], &[]);
        let workers = peer_workers(status, &config).unwrap();
        assert_eq!(workers.len(), 3);
        assert_eq!(workers[1].key, "bsd");
        assert_eq!(workers[1].destination.as_deref(), Some("dev@bsd"));
        assert_eq!(
            explicit_worker("bsd", &config)
                .unwrap()
                .destination
                .as_deref(),
            Some("dev@bsd")
        );
    }

    #[test]
    fn discovery_requires_same_user_unshared_peers_unless_explicitly_configured() {
        let status = r#"{"BackendState":"Running","Self":{"UserID":10},"Peer":{
            "a":{"ID":"owned","DNSName":"owned.tail.test.","OS":"linux","UserID":10},
            "b":{"ID":"shared","DNSName":"shared.tail.test.","OS":"linux","UserID":10,"ShareeNode":true},
            "c":{"ID":"other","DNSName":"other.tail.test.","OS":"macOS","UserID":20},
            "d":{"ID":"unknown","DNSName":"unknown.tail.test.","OS":"macOS"}
        }}"#;
        let workers = peer_workers(status, &Config::default()).unwrap();
        let keys: Vec<_> = workers.iter().map(|worker| worker.key.as_str()).collect();
        assert_eq!(keys, ["owned"]);

        let configured = config(
            &[
                ("shared", "alice@shared-alias"),
                ("other", "alice@other-alias"),
            ],
            &[],
        );
        let workers = peer_workers(status, &configured).unwrap();
        let destinations: Vec<_> = workers
            .iter()
            .map(|worker| worker.destination.as_deref().unwrap())
            .collect();
        assert_eq!(
            destinations,
            ["owned.tail.test", "alice@shared-alias", "alice@other-alias"]
        );
        let ignored = config(&[("shared", "alice@shared-alias")], &["shared"]);
        assert!(
            peer_workers(status, &ignored)
                .unwrap()
                .iter()
                .all(|worker| worker.key != "shared")
        );

        let hosts = vec!["shared.tail.test".into(), "other.tail.test".into()];
        let (workers, warning) = discover(
            &Cancel::new(),
            &Config::default(),
            &Options {
                hosts: hosts.clone(),
                no_local: true,
                ..Options::default()
            },
        )
        .unwrap();
        assert!(warning.is_empty());
        assert_eq!(
            workers.iter().map(|worker| &worker.key).collect::<Vec<_>>(),
            hosts.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn discovery_requires_known_same_nonzero_user_ids() {
        for own in [
            "",
            r#","Self":{}"#,
            r#","Self":{"UserID":0}"#,
            r#","Self":{"UserID":10}"#,
        ] {
            let status = format!(
                r#"{{"BackendState":"Running"{own},"Peer":{{
                    "a":{{"ID":"unknown","DNSName":"unknown.tail.test.","OS":"linux"}},
                    "b":{{"ID":"owned","DNSName":"owned.tail.test.","OS":"linux","UserID":10}},
                    "c":{{"ID":"zero","DNSName":"zero.tail.test.","OS":"linux","UserID":0}},
                    "d":{{"ID":"shared","DNSName":"shared.tail.test.","OS":"linux","UserID":10,"ShareeNode":true}},
                    "e":{{"ID":"other","DNSName":"other.tail.test.","OS":"macOS","UserID":20}}
                }}}}"#
            );
            let workers = peer_workers(&status, &Config::default()).unwrap();
            let keys: Vec<_> = workers.iter().map(|worker| worker.key.as_str()).collect();
            let expected = if own == r#","Self":{"UserID":10}"# {
                vec!["owned"]
            } else {
                vec![]
            };
            assert_eq!(keys, expected, "local ownership: {own}");
        }
    }

    #[test]
    fn ignored_peer_aliases_do_not_reappear_as_configured_workers() {
        let status = r#"{"BackendState":"Running","Self":{"UserID":10},"Peer":{"key":{"ID":"stable","DNSName":"dev.tail.test.","HostName":"hostname","OS":"linux","UserID":10}}}"#;
        for alias in ["stable", "dev.tail.test", "dev", "hostname"] {
            let config = config(&[("stable", "ssh-alias"), ("dev", "other-alias")], &[alias]);
            assert!(
                peer_workers(status, &config).unwrap().is_empty(),
                "ignored alias {alias}"
            );
        }
    }

    #[test]
    fn peer_address_fallback_and_deterministic_ordering() {
        let status = r#"{"BackendState":"Running","Self":{"UserID":10},"Peer":{
            "z":{"OS":"linux","UserID":10,"TailscaleIPs":["100.64.0.2"]},
            "a":{"HostName":"visible name","OS":"macOS","UserID":10,"TailscaleIPs":["fd7a:115c:a1e0::1"]},
            "m":{"OS":"linux","UserID":10}
        }}"#;
        let config = config(
            &[
                ("z-extra", "z-host"),
                ("a-extra", "a-host"),
                ("local", "unused"),
            ],
            &[],
        );
        let workers = peer_workers(status, &config).unwrap();
        let names: Vec<_> = workers.iter().map(|worker| worker.name.as_str()).collect();
        assert_eq!(names, ["visible name", "100.64.0.2", "a-extra", "z-extra"]);
    }

    #[test]
    fn peer_inventory_rejects_malformed_data() {
        for raw in [
            "null",
            "[]",
            "{}",
            r#"{"BackendState":"Stopped"}"#,
            r#"{"BackendState":null}"#,
            r#"{"BackendState":"Running","Self":null}"#,
            r#"{"BackendState":"Running","Self":[]}"#,
            r#"{"BackendState":"Running","Self":{"UserID":null}}"#,
            r#"{"BackendState":"Running","Self":{"UserID":"10"}}"#,
            r#"{"BackendState":"Running","Peer":null}"#,
            r#"{"BackendState":"Running","Peer":[]}"#,
            r#"{"BackendState":"Running","Peer":{"key":null}}"#,
            r#"{"BackendState":"Running","Peer":{"key":[]}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"ID":null}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"DNSName":null}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"HostName":null}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"OS":null}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"Online":null}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"ShareeNode":null}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"ShareeNode":"true"}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"UserID":null}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"UserID":"10"}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"TailscaleIPs":null}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"TailscaleIPs":[null]}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"TailscaleIPs":[42]}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"DNSName":42}}}"#,
            r#"{"BackendState":"Running","Peer":{"key":{"Online":"yes"}}}"#,
            r#"{"BackendState":"Running","Self":{"UserID":10},"Peer":{"key":{"OS":"linux","UserID":10,"DNSName":"host;id"}}}"#,
            r#"{"BackendState":"Running"} {}"#,
        ] {
            assert!(
                peer_workers(raw, &Config::default()).is_err(),
                "accepted malformed inventory: {raw}"
            );
        }
        assert!(
            peer_workers(r#"{"BackendState":"Running"}"#, &Config::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn explicit_hosts_bypass_discovery_and_deduplicate_destinations() {
        let config = config(
            &[("alias", "user@host"), ("unused", "unused-host")],
            &["alias", "user@host"],
        );
        let cancel = Cancel::new();
        let (workers, warning) = discover(
            &cancel,
            &config,
            &Options {
                hosts: vec!["alias".into(), "user@host".into(), "local".into()],
                ..Options::default()
            },
        )
        .unwrap();
        assert!(warning.is_empty());
        assert_eq!(workers.len(), 2);
        assert_eq!(workers[0], local_worker());
        assert_eq!(workers[1].key, "user@host");
        let (workers, _) = discover(
            &cancel,
            &config,
            &Options {
                hosts: vec!["alias".into()],
                no_local: true,
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].key, "alias");
        let (workers, _) = discover(
            &cancel,
            &config,
            &Options {
                hosts: vec!["-invalid".into()],
                local_only: true,
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(workers, [local_worker()]);
        let (workers, _) = discover(
            &cancel,
            &config,
            &Options {
                local_only: true,
                no_local: true,
                ..Options::default()
            },
        )
        .unwrap();
        assert!(workers.is_empty());
        assert!(
            discover(
                &cancel,
                &config,
                &Options {
                    hosts: vec!["-invalid".into()],
                    ..Options::default()
                }
            )
            .is_err(),
            "invalid explicit computer silently fell back"
        );
    }

    #[test]
    fn discovery_failure_retains_configured_workers() {
        let _env = env_write();
        let dir = TempDir::new();
        let _path = EnvVar::set("PATH", dir.path());
        if tailscale_command().is_some() {
            eprintln!("skipped: macOS application fallback is installed");
            return;
        }
        let config = config(
            &[
                ("z", "same-host"),
                ("a", "same-host"),
                ("ignore", "ignored-host"),
                ("local", "unused"),
            ],
            &["ignore"],
        );
        let (workers, warning) = discover(&Cancel::new(), &config, &Options::default()).unwrap();
        assert!(!warning.is_empty());
        assert_eq!(workers.len(), 2);
        assert_eq!(workers[0].key, "local");
        assert_eq!(workers[1].key, "z");
        assert_eq!(workers[1].destination.as_deref(), Some("same-host"));
    }

    #[test]
    fn discovery_accepts_sixteen_mib_and_rejects_larger_output() {
        use std::os::unix::fs::PermissionsExt;

        let _env = env_write();
        let dir = TempDir::new();
        let command = dir.path().join("tailscale");
        let inventory = dir.path().join("tailscale.json");
        std::fs::write(&command, "#!/bin/sh\nexec /bin/cat \"$0.json\"\n").unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _path = EnvVar::set("PATH", dir.path());
        let raw = r#"{"BackendState":"Running","Peer":{}}"#;
        let mut output = raw.to_owned();
        output.extend(std::iter::repeat_n(' ', DISCOVERY_OUTPUT_LIMIT - raw.len()));
        std::fs::write(&inventory, &output).unwrap();
        let (workers, warning) =
            discover(&Cancel::new(), &Config::default(), &Options::default()).unwrap();
        assert!(warning.is_empty(), "{warning}");
        assert_eq!(workers, [local_worker()]);

        output.push(' ');
        std::fs::write(&inventory, &output).unwrap();
        let (_, warning) =
            discover(&Cancel::new(), &Config::default(), &Options::default()).unwrap();
        assert!(!warning.is_empty());
    }

    #[test]
    fn discover_preserves_safe_defaults_and_explicit_config_with_restricted_inventory() {
        use std::os::unix::fs::PermissionsExt;

        let _env = env_write();
        let dir = TempDir::new();
        let command = dir.path().join("tailscale");
        let inventory = dir.path().join("tailscale.json");
        std::fs::write(&command, "#!/bin/sh\nexec /bin/cat \"$0.json\"\n").unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _path = EnvVar::set("PATH", dir.path());
        for (own, eligible) in [
            (r#","Self":{"UserID":10}"#, true),
            ("", false),
            (r#","Self":{}"#, false),
            (r#","Self":{"UserID":0}"#, false),
            (r#","Self":{"UserID":10,"Tags":["tag:client"]}"#, false),
        ] {
            let raw = format!(
                r#"{{"BackendState":"Running"{own},"Peer":{{
                    "a":{{"ID":"owned","DNSName":"owned.tail.test.","OS":"macOS","UserID":10,"Online":false}},
                    "b":{{"ID":"unknown-id","DNSName":"dev.foreign.test.","HostName":"dev","OS":"linux","Online":true}},
                    "c":{{"ID":"tagged-id","DNSName":"tagged.tail.test.","OS":"linux","UserID":10,"Tags":["tag:server"]}},
                    "d":{{"ID":"shared","DNSName":"shared.tail.test.","OS":"linux","UserID":10,"ShareeNode":true}},
                    "e":{{"ID":"other","DNSName":"other.tail.test.","OS":"linux","UserID":20}},
                    "f":{{"ID":"zero","DNSName":"zero.tail.test.","OS":"linux","UserID":0}},
                    "g":{{"ID":"ignored-id","DNSName":"ignored.tail.test.","OS":"linux"}}
                }}}}"#
            );
            std::fs::write(&inventory, raw).unwrap();
            let (workers, warning) =
                discover(&Cancel::new(), &Config::default(), &Options::default()).unwrap();
            assert!(warning.is_empty(), "{warning}");
            assert_eq!(workers.len(), 1 + usize::from(eligible), "{own}");
            assert_eq!(workers[0], local_worker());
            if eligible {
                assert_eq!(workers[1].key, "owned");
                assert_eq!(workers[1].destination.as_deref(), Some("owned.tail.test"));
                assert_eq!(workers[1].online, Some(false));
            }

            let configured = config(
                &[
                    ("unknown-id", "alice@unknown"),
                    ("tagged.tail.test", "alice@tagged"),
                    ("dev", "safe-alias"),
                    ("absent", "absent-alias"),
                    ("ignored-id", "ignored-alias"),
                ],
                &["ignored.tail.test"],
            );
            let (workers, warning) =
                discover(&Cancel::new(), &configured, &Options::default()).unwrap();
            assert!(warning.is_empty(), "{warning}");
            let mut expected = vec![local_worker()];
            if eligible {
                expected.push(Worker {
                    key: "owned".into(),
                    name: "owned".into(),
                    destination: Some("owned.tail.test".into()),
                    online: Some(false),
                });
            }
            expected.extend([
                Worker {
                    key: "unknown-id".into(),
                    name: "dev".into(),
                    destination: Some("alice@unknown".into()),
                    online: Some(true),
                },
                Worker {
                    key: "tagged-id".into(),
                    name: "tagged".into(),
                    destination: Some("alice@tagged".into()),
                    online: None,
                },
                explicit_worker("absent", &configured).unwrap(),
                explicit_worker("dev", &configured).unwrap(),
            ]);
            assert_eq!(workers, expected, "{own}");
        }
    }

    #[test]
    fn discovery_requires_untagged_local_and_peer_even_with_matching_owners() {
        for (own_tags, peer_tags, untagged) in [
            ("[]", "[]", true),
            ("null", "null", true),
            ("[]", r#"["tag:server"]"#, false),
            (r#"["tag:client"]"#, "[]", false),
            (r#"["tag:client"]"#, r#"["tag:server"]"#, false),
        ] {
            for user in [0, 10, 20] {
                for shared in [false, true] {
                    let status = format!(
                        r#"{{"BackendState":"Running","Self":{{"UserID":10,"Tags":{own_tags}}},
                        "Peer":{{"a":{{"ID":"node-id","DNSName":"dev.tail.test.","OS":"linux",
                        "UserID":{user},"Tags":{peer_tags},"ShareeNode":{shared}}}}}}}"#
                    );
                    let workers = peer_workers(&status, &Config::default()).unwrap();
                    assert_eq!(
                        workers.len(),
                        usize::from(untagged && user == 10 && !shared),
                        "{status}"
                    );
                    if let Some(worker) = workers.first() {
                        assert_eq!(worker.key, "node-id");
                        assert_eq!(worker.destination.as_deref(), Some("dev.tail.test"));
                    }
                }
            }
        }
    }

    #[test]
    fn restricted_peers_cannot_claim_hostname_or_short_dns_mapping_or_ignore() {
        for (own, restriction) in [
            (r#"{"UserID":10}"#, r#""UserID":20"#),
            (r#"{"UserID":10}"#, r#""UserID":10,"ShareeNode":true"#),
            (r#"{"UserID":10}"#, r#""UserID":10,"Tags":["tag:server"]"#),
            (r#"{"UserID":10}"#, r#""UserID":0"#),
            (r#"{"UserID":10}"#, r#""Tags":[]"#),
            (r#"{}"#, r#""UserID":10"#),
            (r#"{"UserID":0}"#, r#""UserID":0"#),
            (r#"{"UserID":10,"Tags":["tag:client"]}"#, r#""UserID":10"#),
        ] {
            let status = format!(
                r#"{{"BackendState":"Running","Self":{own},"Peer":{{
                "a":{{"ID":"node-id","DNSName":"dev.foreign.test.","HostName":"trusted",
                "OS":"linux",{restriction}}}}}}}"#
            );
            for alias in ["trusted", "dev"] {
                // An untrusted alias must not consume a separately configured host.
                let workers =
                    peer_workers(&status, &config(&[(alias, "safe-destination")], &[])).unwrap();
                assert_eq!(workers.len(), 1);
                assert_eq!(workers[0].key, alias);
                assert_eq!(workers[0].online, None);

                // Nor may that alias ignore an explicitly opted-in peer.
                let workers = peer_workers(
                    &status,
                    &config(&[("node-id", "explicit-destination")], &[alias]),
                )
                .unwrap();
                assert_eq!(workers.len(), 1);
                assert_eq!(workers[0].key, "node-id");
                assert_eq!(
                    workers[0].destination.as_deref(),
                    Some("explicit-destination")
                );
            }
            for alias in ["node-id", "dev.foreign.test"] {
                let workers =
                    peer_workers(&status, &config(&[(alias, "explicit-destination")], &[]))
                        .unwrap();
                assert_eq!(workers.len(), 1);
                assert_eq!(workers[0].key, "node-id");
                assert_eq!(
                    workers[0].destination.as_deref(),
                    Some("explicit-destination")
                );
                assert!(
                    peer_workers(
                        &status,
                        &config(&[("node-id", "explicit-destination")], &[alias]),
                    )
                    .unwrap()
                    .is_empty()
                );
            }
        }
    }

    #[test]
    fn malformed_tags_reject_inventory() {
        for tags in [r#""tag:server""#, "[1]", "{}"] {
            for location in ["Self", "Peer"] {
                let status = if location == "Self" {
                    format!(r#"{{"BackendState":"Running","Self":{{"Tags":{tags}}}}}"#)
                } else {
                    format!(r#"{{"BackendState":"Running","Peer":{{"a":{{"Tags":{tags}}}}}}}"#)
                };
                assert!(peer_workers(&status, &Config::default()).is_err());
            }
        }
    }
}
