use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr, SocketAddrV6, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::{Deserialize, Serialize};

use crate::cancel::Cancel;
use crate::process::run_command;
use crate::workers::{Config, Worker, validate_destination};

const SERVICE_TYPE: &str = "_ssh._tcp.local.";
const BROWSE_TIME: Duration = Duration::from_millis(1200);
const ROUTE_TIME: Duration = Duration::from_secs(3);
const MAX_SERVICES: usize = 128;
const MAX_ENDPOINTS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Address {
    pub hostname: String,
    #[serde(default = "ssh_port")]
    pub port: u16,
}

fn ssh_port() -> u16 {
    22
}

impl Address {
    pub(crate) fn validate(&self) -> Result<(), String> {
        validate_destination(&self.hostname)?;
        if self.port == 0 || self.hostname.contains('@') {
            return Err(
                "LAN addresses require a hostname (without a username) and port 1–65535".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Service {
    hostname: String,
    addresses: Vec<Address>,
}

fn accumulate(services: &mut BTreeMap<String, Service>, service: Service) {
    if services.len() >= MAX_SERVICES && !services.contains_key(&service.hostname) {
        return;
    }
    let addresses = &mut services
        .entry(service.hostname.clone())
        .or_insert_with(|| Service {
            hostname: service.hostname,
            addresses: Vec::new(),
        })
        .addresses;
    addresses.extend(service.addresses);
    // Include the port in ordering and identity: one hostname can advertise
    // multiple SSH listeners, and records can arrive in either order.
    addresses.sort_by(|a, b| {
        (a.hostname.contains(':'), &a.hostname, a.port).cmp(&(
            b.hostname.contains(':'),
            &b.hostname,
            b.port,
        ))
    });
    addresses.dedup();
    addresses.truncate(MAX_ENDPOINTS);
}

// A browse is local multicast only, bounded in time and result count, and never
// started by scoped --host/--local requests. Dropping the receiver is not enough
// to stop mdns-sd's thread: always explicitly shut its daemon down.
pub(crate) fn browse(cancel: &Cancel) -> Result<Vec<Service>, String> {
    if cancel.is_cancelled() {
        return Ok(Vec::new());
    }
    let daemon = ServiceDaemon::new().map_err(|error| format!("LAN discovery: {error}"))?;
    let result = (|| {
        let receiver = daemon
            .browse(SERVICE_TYPE)
            .map_err(|error| format!("LAN discovery: {error}"))?;
        let deadline = Instant::now() + BROWSE_TIME;
        let mut services = BTreeMap::new();
        while !cancel.is_cancelled() && Instant::now() < deadline {
            let wait =
                Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now()));
            if let Ok(ServiceEvent::ServiceResolved(service)) = receiver.recv_timeout(wait) {
                let hostname = service
                    .get_hostname()
                    .trim_end_matches('.')
                    .to_ascii_lowercase();
                if !hostname.ends_with(".local") || validate_destination(&hostname).is_err() {
                    continue;
                }
                let addresses: Vec<_> = service
                    .get_addresses()
                    .iter()
                    .filter(|address| {
                        !address.is_loopback()
                            && !address.to_ip_addr().is_unspecified()
                            && !address.to_ip_addr().is_multicast()
                    })
                    .map(|address| Address {
                        hostname: address.to_string(),
                        port: service.get_port(),
                    })
                    .filter(|address| address.validate().is_ok())
                    .collect();
                if !addresses.is_empty() {
                    accumulate(
                        &mut services,
                        Service {
                            hostname,
                            addresses,
                        },
                    );
                }
            }
        }
        Ok(services.into_values().collect())
    })();
    let _ = daemon.stop_browse(SERVICE_TYPE);
    if let Ok(stopped) = daemon.shutdown() {
        let _ = stopped.recv_timeout(Duration::from_millis(100));
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    addresses: Vec<Address>,
    // A newly discovered computer has no other identity to inherit. Its own
    // hostname/advertised port is the explicit host-trust identity instead.
    new_service: bool,
}

#[derive(Default)]
struct Route {
    candidate: Option<Arc<Candidate>>,
    revision: u64,
    prepared: bool,
    preparing: bool,
    options: Vec<String>,
    used: BTreeSet<Vec<String>>,
}

static ROUTES: Mutex<BTreeMap<String, Arc<Mutex<Route>>>> = Mutex::new(BTreeMap::new());

fn host(destination: &str) -> &str {
    destination
        .rsplit_once('@')
        .map_or(destination, |(_, host)| host)
}

fn short(hostname: &str) -> &str {
    hostname.split('.').next().unwrap_or(hostname)
}

fn plan(
    workers: &mut Vec<Worker>,
    config: &Config,
    services: Vec<Service>,
) -> BTreeMap<String, Arc<Candidate>> {
    let mut routes = BTreeMap::new();
    for worker in workers.iter() {
        if let Some(destination) = &worker.destination
            && let Some(address) = config.lan.get(destination)
        {
            routes.insert(
                destination.clone(),
                Arc::new(Candidate {
                    addresses: vec![address.clone()],
                    new_service: false,
                }),
            );
        }
    }
    for service in services {
        if config
            .ignore
            .iter()
            .any(|name| name == &service.hostname || name == short(&service.hostname))
            || workers.iter().any(|worker| {
                worker.destination.is_none()
                    && worker.name.eq_ignore_ascii_case(short(&service.hostname))
            })
        {
            continue;
        }
        let matches: BTreeSet<_> = workers
            .iter()
            .filter(|worker| {
                worker.destination.as_deref().is_some_and(|destination| {
                    host(destination).eq_ignore_ascii_case(&service.hostname)
                        || short(host(destination)).eq_ignore_ascii_case(short(&service.hostname))
                        || worker.name.eq_ignore_ascii_case(short(&service.hostname))
                        || service
                            .addresses
                            .iter()
                            .any(|address| host(destination) == address.hostname)
                        || config.lan.get(destination).is_some_and(|address| {
                            address.hostname.eq_ignore_ascii_case(&service.hostname)
                                || service.addresses.contains(address)
                        })
                })
            })
            .filter_map(|worker| worker.destination.as_ref())
            .collect();
        if !matches.is_empty() {
            // A name is only a nomination, never evidence of SSH identity. For
            // ambiguous names do not guess, and do not add duplicate computers.
            if matches.len() == 1 {
                let destination = *matches.first().unwrap();
                routes.entry(destination.clone()).or_insert_with(|| {
                    Arc::new(Candidate {
                        addresses: service.addresses,
                        new_service: false,
                    })
                });
            }
            continue;
        }
        let destination = service.hostname;
        routes.insert(
            destination.clone(),
            Arc::new(Candidate {
                addresses: service.addresses,
                new_service: true,
            }),
        );
        workers.push(Worker {
            key: destination.clone(),
            name: short(&destination).into(),
            destination: Some(destination),
            online: Some(true),
        });
    }
    routes
}

pub(crate) fn merge(workers: &mut Vec<Worker>, config: &Config, services: Vec<Service>) {
    let planned = plan(workers, config, services);
    let mut routes = ROUTES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Retain used routes until mux shutdown, including candidates which vanish.
    for worker in workers {
        let Some(destination) = &worker.destination else {
            continue;
        };
        let candidate = planned.get(destination).cloned();
        let route = routes.entry(destination.clone()).or_default();
        let mut route = route
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        route.candidate = candidate;
        route.revision = route.revision.wrapping_add(1);
        route.prepared = false;
    }
}

fn route(destination: &str) -> Option<Arc<Mutex<Route>>> {
    ROUTES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(destination)
        .cloned()
}

pub(crate) fn clear() {
    ROUTES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

pub(crate) fn invalidate(destination: &str) {
    if let Some(route) = route(destination) {
        let mut route = route
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        route.revision = route.revision.wrapping_add(1);
        route.prepared = false;
    }
}

pub(crate) fn add_options(args: &mut Vec<String>, destination: &str) {
    if let Some(route) = route(destination) {
        args.extend(
            route
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .options
                .iter()
                .cloned(),
        );
    }
}

pub(crate) fn used_options(destination: &str) -> Vec<Vec<String>> {
    let mut options = vec![Vec::new()];
    if let Some(route) = route(destination) {
        options.extend(
            route
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .used
                .iter()
                .cloned(),
        );
    }
    options
}

fn settings(
    cancel: &Cancel,
    destination: &str,
    options: &[String],
    timeout: Duration,
) -> Option<BTreeMap<String, String>> {
    let mut args = vec!["ssh".into(), "-G".into()];
    args.extend_from_slice(options);
    args.extend(["--".into(), destination.into()]);
    let result = run_command(cancel, &args, timeout, false).ok()?;
    if result.code != 0 {
        return None;
    }
    let mut settings = BTreeMap::<String, String>::new();
    for line in result.stdout.lines() {
        if let Some((key, value)) = line.split_once(' ') {
            settings
                .entry(key.into())
                .and_modify(|old| {
                    old.push('\n');
                    old.push_str(value);
                })
                .or_insert_with(|| value.into());
        }
    }
    Some(settings)
}

fn route_options(
    settings: &BTreeMap<String, String>,
    address: &Address,
    new_service: bool,
) -> Option<Vec<String>> {
    for key in ["proxyjump", "proxycommand"] {
        if settings.get(key).is_some_and(|value| value != "none") {
            return None;
        }
    }
    if settings
        .get("canonicalizehostname")
        .is_some_and(|value| value != "false" && value != "no")
    {
        return None;
    }
    // Expansions would otherwise silently change identity files, agent sockets,
    // known-host files or local commands when HostName/Port changes.
    if settings.values().any(|value| {
        ["%h", "%p", "%C", "%k"]
            .iter()
            .any(|token| value.contains(token))
    }) {
        return None;
    }
    let hostname = settings.get("hostname")?;
    let port: u16 = settings.get("port")?.parse().ok()?;
    let alias = settings
        .get("hostkeyalias")
        .filter(|value| value.as_str() != "none")
        .cloned()
        .unwrap_or_else(|| {
            let port = if new_service { address.port } else { port };
            if port == 22 {
                hostname.clone()
            } else {
                format!("[{hostname}]:{port}")
            }
        });
    if alias.chars().any(char::is_whitespace) {
        return None;
    }
    // HostName is an ssh_config value: escape interface-zone '%' characters
    // rather than letting them become OpenSSH expansion tokens.
    let route_hostname = if address.hostname.contains('%') {
        std::borrow::Cow::Owned(address.hostname.replace('%', "%%"))
    } else {
        std::borrow::Cow::Borrowed(address.hostname.as_str())
    };
    let mut options = Vec::new();
    for value in [
        format!("HostName={route_hostname}"),
        format!("Port={}", address.port),
        format!("HostKeyAlias={alias}"),
        "CheckHostIP=no".into(),
        "UpdateHostKeys=no".into(),
        "VerifyHostKeyDNS=no".into(),
    ] {
        options.extend(["-o".into(), value]);
    }
    if !new_service {
        options.extend(["-o".into(), "StrictHostKeyChecking=yes".into()]);
    }
    Some(options)
}

fn compatible(original: &BTreeMap<String, String>, routed: &BTreeMap<String, String>) -> bool {
    let unchanged = |key: &str| {
        ![
            "hostname",
            "port",
            "hostkeyalias",
            "checkhostip",
            "updatehostkeys",
            "verifyhostkeydns",
            "stricthostkeychecking",
        ]
        .contains(&key)
    };
    original
        .iter()
        .filter(|(key, _)| unchanged(key))
        .eq(routed.iter().filter(|(key, _)| unchanged(key)))
}

// Select/authenticate before an inventory command, not inside the command
// runner: create/end/attach are never replayed on another route. A successful
// command-free master is reused by inventory and subsequent SSH operations.
pub(crate) fn prepare(cancel: &Cancel, destination: &str, interactive: bool) {
    let Some(route) = route(destination) else {
        return;
    };
    let (candidate, revision) = {
        let mut route = route
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if route.preparing
            || (route.prepared && !(interactive && route.options.is_empty()))
            || cancel.is_cancelled()
        {
            return;
        }
        route.prepared = true;
        route.preparing = true;
        (route.candidate.clone(), route.revision)
    };
    // Do not hold a route lock while doing SSH or TCP I/O. The UI's embedded
    // sign-in reads options synchronously, and must never wait for a probe.
    let mut used = BTreeSet::new();
    let options = candidate.map_or_else(Vec::new, |candidate| {
        select(cancel, destination, &candidate, &mut used)
    });
    let mut route = route
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    route.used.extend(used);
    route.preparing = false;
    if cancel.is_cancelled() {
        route.prepared = false;
    }
    if route.revision == revision && !cancel.is_cancelled() {
        route.options = options;
    }
}

fn advertised_socket(address: &Address) -> Option<SocketAddr> {
    let hostname = address
        .hostname
        .trim_start_matches('[')
        .trim_end_matches(']');
    if let Some((ip, zone)) = hostname.split_once('%') {
        let ip = ip.parse().ok()?;
        let scope = zone.parse().ok().or_else(|| {
            let zone = std::ffi::CString::new(zone).ok()?;
            // SAFETY: zone is a valid, NUL-terminated interface name.
            let index = unsafe { libc::if_nametoindex(zone.as_ptr()) };
            (index != 0).then_some(index)
        })?;
        Some(SocketAddrV6::new(ip, address.port, 0, scope).into())
    } else {
        Some(SocketAddr::new(
            hostname.parse::<IpAddr>().ok()?,
            address.port,
        ))
    }
}

fn reachable(cancel: &Cancel, address: &Address, deadline: Instant) -> bool {
    let Some(socket) = advertised_socket(address) else {
        return false;
    };
    let timeout =
        Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now()));
    !cancel.is_cancelled()
        && !timeout.is_zero()
        && TcpStream::connect_timeout(&socket, timeout).is_ok()
}

fn select(
    cancel: &Cancel,
    destination: &str,
    candidate: &Candidate,
    used: &mut BTreeSet<Vec<String>>,
) -> Vec<String> {
    let deadline = Instant::now() + ROUTE_TIME;
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let Some(original) = settings(cancel, destination, &[], remaining()) else {
        return Vec::new();
    };
    let mut unavailable = None;
    for address in &candidate.addresses {
        if cancel.is_cancelled() || remaining().is_zero() {
            break;
        }
        let Some(options) = route_options(&original, address, candidate.new_service) else {
            continue;
        };
        let Some(routed) = settings(cancel, destination, &options, remaining()) else {
            continue;
        };
        if !compatible(&original, &routed) {
            continue;
        }
        if candidate.new_service {
            // Reachability is not host trust. The eventual command still uses
            // the normal explicit ask flow; this sends no SSH authentication.
            if reachable(cancel, address, deadline) {
                used.insert(options.clone());
                return options;
            }
            // If every advertised endpoint is down, report failure on one of
            // those endpoints, not an unadvertised default-port hostname.
            unavailable.get_or_insert(options);
            continue;
        }
        let mut args: Vec<String> = [
            "ssh",
            "-f",
            "-N",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=1",
            "-o",
            "ConnectionAttempts=1",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ForwardAgent=no",
            "-o",
            "ForwardX11=no",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "RemoteCommand=none",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        args.extend_from_slice(&options);
        crate::ssh_mux::add_options(&mut args, destination);
        if !args.iter().any(|arg| arg == "ControlMaster=auto") {
            break;
        }
        args.extend(["--".into(), destination.into()]);
        used.insert(options.clone());
        if let Ok(result) = run_command(cancel, &args, remaining(), false)
            && (result.code == 0
                || crate::app::failure(result.code, &result.stderr).0
                    == crate::app::Status::AuthRequired)
        {
            // AuthRequired only nominates a route for embedded sign-in; it is
            // NOT proof of trust. StrictHostKeyChecking=yes remains mandatory
            // on that new SSH connection, before any credential or command.
            return options;
        }
    }
    unavailable.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(hostname: &str) -> Address {
        Address {
            hostname: hostname.into(),
            port: 22,
        }
    }

    fn worker() -> Worker {
        Worker {
            key: "stable-node-id".into(),
            name: "dev".into(),
            destination: Some("alice@dev.tail.test".into()),
            online: Some(false),
        }
    }

    fn service() -> Service {
        Service {
            hostname: "dev.local".into(),
            addresses: vec![address("192.0.2.1")],
        }
    }

    fn ssh_settings() -> BTreeMap<String, String> {
        [
            ("hostname", "dev.tail.test"),
            ("port", "2222"),
            ("user", "alice"),
            ("identityfile", "~/.ssh/dev"),
            ("canonicalizehostname", "false"),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
    }

    #[test]
    fn repeated_records_accumulate_ports_and_addresses_in_deterministic_order() {
        let first = service();
        let second = Service {
            hostname: first.hostname.clone(),
            addresses: vec![
                address("2001:db8::1"),
                Address {
                    hostname: "192.0.2.1".into(),
                    port: 2222,
                },
                address("192.0.2.2"),
            ],
        };
        let mut forward = BTreeMap::new();
        let mut reverse = BTreeMap::new();
        for item in [first.clone(), second.clone(), first.clone()] {
            accumulate(&mut forward, item);
        }
        for item in [second, first] {
            accumulate(&mut reverse, item);
        }
        let addresses = &forward["dev.local"].addresses;
        assert_eq!(addresses, &reverse["dev.local"].addresses);
        assert_eq!(addresses.len(), 4);
        assert_eq!(addresses[0], address("192.0.2.1"));
        assert_eq!(addresses[1].port, 2222);
        let mut workers = vec![worker()];
        let planned = plan(
            &mut workers,
            &Config::default(),
            forward.into_values().collect(),
        );
        assert_eq!(workers.len(), 1);
        assert_eq!(planned["alice@dev.tail.test"].addresses.len(), 4);
    }

    #[test]
    fn advertised_socket_preserves_ipv6_interface_scope() {
        let address = Address {
            hostname: "fe80::1%7".into(),
            port: 2222,
        };
        let socket = advertised_socket(&address).unwrap();
        let SocketAddr::V6(socket) = socket else {
            panic!("expected IPv6");
        };
        assert_eq!(socket.scope_id(), 7);
        assert_eq!(socket.port(), 2222);
    }

    #[test]
    fn hostname_only_nominates_route_and_preserves_identity() {
        let original = worker();
        let mut workers = vec![original.clone()];
        let planned = plan(&mut workers, &Config::default(), vec![service(), service()]);
        assert_eq!(workers, [original]);
        assert_eq!(planned.len(), 1);
        assert!(!planned["alice@dev.tail.test"].new_service);
    }

    #[test]
    fn duplicate_configured_aliases_do_not_make_one_destination_ambiguous() {
        let first = worker();
        let mut second = first.clone();
        second.name = "second local label".into();
        let mut workers = vec![first, second];
        let planned = plan(&mut workers, &Config::default(), vec![service()]);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned["alice@dev.tail.test"].addresses.len(), 1);
        assert_eq!(workers.len(), 2, "discovery added a duplicate LAN computer");
    }

    #[test]
    fn explicit_mapping_wins_and_nonadvertising_hosts_work() {
        let mut config = Config::default();
        config
            .lan
            .insert("alice@dev.tail.test".into(), address("192.0.2.2"));
        for services in [vec![], vec![service()]] {
            let planned = plan(&mut vec![worker()], &config, services);
            assert_eq!(
                planned["alice@dev.tail.test"].addresses,
                [address("192.0.2.2")]
            );
        }
    }

    #[test]
    fn new_services_need_their_own_trust_and_ambiguous_names_are_not_paired() {
        let mut workers = Vec::new();
        let planned = plan(&mut workers, &Config::default(), vec![service(), service()]);
        assert_eq!(workers.len(), 1);
        assert!(planned["dev.local"].new_service);
        let mut second = worker();
        second.destination = Some("bob@dev.other.test".into());
        let planned = plan(
            &mut vec![worker(), second],
            &Config::default(),
            vec![service()],
        );
        assert!(planned.is_empty());
    }

    #[test]
    fn alternate_routes_reuse_original_port_and_host_key_alias() {
        let mut settings = ssh_settings();
        let options = route_options(&settings, &address("192.0.2.1"), false).unwrap();
        assert!(options.contains(&"HostKeyAlias=[dev.tail.test]:2222".into()));
        assert!(options.contains(&"UpdateHostKeys=no".into()));
        settings.insert("hostkeyalias".into(), "pinned-name".into());
        assert!(
            route_options(&settings, &address("192.0.2.1"), false)
                .unwrap()
                .contains(&"HostKeyAlias=pinned-name".into())
        );
    }

    #[test]
    fn proxy_and_route_sensitive_settings_prevent_bypass() {
        for (key, value) in [
            ("proxyjump", "bastion"),
            ("proxycommand", "custom %h"),
            ("identityfile", "~/.ssh/%h"),
            ("canonicalizehostname", "true"),
        ] {
            let mut settings = ssh_settings();
            settings.insert(key.into(), value.into());
            assert!(route_options(&settings, &address("192.0.2.1"), false).is_none());
        }
        let original = ssh_settings();
        let mut changed = original.clone();
        changed.insert("identityfile".into(), "~/.ssh/different".into());
        assert!(!compatible(&original, &changed));
    }
}

#[cfg(test)]
#[path = "lan_tests.rs"]
mod integration_tests;
