use super::*;
use crate::embedded::EmbeddedTerminal;
use crate::test_support::{EnvVar, TempDir, env_write};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        crate::ssh_mux::close();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn server(root: &Path, key: &str, user: &str) -> (Server, u16) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = root.join(format!("{key}.config"));
    let log = root.join(format!("{key}.log"));
    let directory = root.display();
    fs::write(
        &config,
        format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {directory}/{key}\n\
             PidFile {directory}/{key}.pid\nAuthorizedKeysFile {directory}/client.pub\n\
             StrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n\
             UsePAM no\nAllowUsers {user}\nLogLevel DEBUG3\n"
        ),
    )
    .unwrap();
    drop(listener);
    let child = Command::new("/usr/sbin/sshd")
        .args(["-D", "-e", "-f"])
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut server = Server(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            server.0.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "{}",
            fs::read_to_string(&log).unwrap()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    (server, port)
}

fn client_config(root: &Path, user: &str, canonical_port: u16) {
    let directory = root.display();
    fs::write(
        root.join("ssh_config"),
        format!(
            "Host delux-lan-fixture\nHostName 127.0.0.1\nPort {canonical_port}\n\
             HostKeyAlias fixture-original\n\
             Host *\nUser {user}\nIdentityFile {directory}/client\n\
             IdentityAgent none\nIdentitiesOnly yes\n\
             UserKnownHostsFile {directory}/known_hosts\nGlobalKnownHostsFile /dev/null\n"
        ),
    )
    .unwrap();
}

fn configured_route(worker: &Worker, port: u16) {
    let mut config = Config::default();
    config.lan.insert(
        worker.destination.as_ref().unwrap().clone(),
        Address {
            hostname: "127.0.0.1".into(),
            port,
        },
    );
    merge(&mut vec![worker.clone()], &config, Vec::new());
}

// Reserve a TCP port without listening. Connections fail on our own bound
// socket; no race can redirect a test to an unrelated process's listener.
fn refused_endpoint() -> TcpListener {
    use rustix::net::{AddressFamily, SocketType, bind, socket};

    let socket = socket(AddressFamily::INET, SocketType::STREAM, None).unwrap();
    bind(
        &socket,
        &std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0),
    )
    .unwrap();
    TcpListener::from(socket)
}

fn new_service_tries_other_addresses_without_accepting_unknown_keys(root: &Path, port: u16) {
    crate::ssh_mux::begin();
    let refused = refused_endpoint();
    let mut workers = Vec::new();
    merge(
        &mut workers,
        &Config::default(),
        vec![Service {
            hostname: "delux-new-fixture.local".into(),
            addresses: vec![
                Address {
                    hostname: "127.0.0.1".into(),
                    port: refused.local_addr().unwrap().port(),
                },
                Address {
                    hostname: "127.0.0.1".into(),
                    port,
                },
            ],
        }],
    );
    let worker = &workers[0];
    let cancel = Cancel::new();
    prepare(&cancel, worker.destination.as_deref().unwrap(), false);
    let command = crate::app::ssh_command(worker, "printf never-authenticated", false, false);
    assert!(command.contains(&format!("Port={port}")));
    let before = fs::read(root.join("known_hosts")).unwrap();
    let result = run_command(&cancel, &command, Duration::from_secs(5), false).unwrap();
    assert_eq!(
        crate::app::failure(result.code, &result.stderr).0,
        crate::app::Status::HostTrust,
        "{result:?}"
    );
    assert!(result.stdout.is_empty());
    assert_eq!(fs::read(root.join("known_hosts")).unwrap(), before);
    crate::ssh_mux::close();
}

fn inline_sign_in_retains_lan_for_locked_keys(
    root: &Path,
    user: &str,
    worker: &Worker,
    lan_port: u16,
) {
    let output = Command::new("/usr/bin/ssh-keygen")
        .args(["-q", "-p", "-P", "", "-N", "fixture-passphrase", "-f"])
        .arg(root.join("client"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let refused = refused_endpoint();
    client_config(root, user, refused.local_addr().unwrap().port());
    crate::ssh_mux::begin();
    configured_route(worker, lan_port);
    let cancel = Cancel::new();
    let destination = worker.destination.as_deref().unwrap();
    prepare(&cancel, destination, false);
    let batch = crate::app::ssh_command(worker, "printf background", false, false);
    assert!(batch.contains(&format!("Port={lan_port}")));
    let result = run_command(&cancel, &batch, Duration::from_secs(5), false).unwrap();
    assert_eq!(
        crate::app::failure(result.code, &result.stderr).0,
        crate::app::Status::AuthRequired,
        "{result:?}"
    );
    // This is the background runner's failure cleanup, then the exact
    // embedded-launch path: no interactive prepare call can rescue it.
    crate::ssh_mux::reset_destination(destination);
    let attachment = crate::ssh_mux::begin_attachment();
    let args = crate::app::computer_command(worker, "printf inline-authenticated", true, false);
    assert!(args.contains(&"StrictHostKeyChecking=yes".into()));
    assert!(args.contains(&format!("Port={lan_port}")));
    let mut terminal = EmbeddedTerminal::spawn_captured(&args, 24, 100).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut entered_passphrase = false;
    loop {
        if let Some(code) = terminal.poll().unwrap() {
            assert_eq!(code, 0, "{}", terminal.contents());
            break;
        }
        if !entered_passphrase && terminal.contents().contains("Enter passphrase") {
            terminal.paste("fixture-passphrase").unwrap();
            terminal
                .send_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap();
            entered_passphrase = true;
        }
        assert!(Instant::now() < deadline, "{}", terminal.contents());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(entered_passphrase);
    assert_eq!(terminal.captured_output(), b"inline-authenticated");
    drop(terminal);
    drop(attachment);
    crate::ssh_mux::close();
}

fn preparing_does_not_block_route_readers_or_publish_obsolete_results(worker: &Worker, port: u16) {
    crate::ssh_mux::begin();
    let destination = worker.destination.as_deref().unwrap();
    let cancel = Cancel::new();
    configured_route(worker, port);
    prepare(&cancel, destination, false);
    let silent = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    silent.set_nonblocking(true).unwrap();
    configured_route(worker, silent.local_addr().unwrap().port());
    std::thread::scope(|scope| {
        let preparing = scope.spawn(|| prepare(&cancel, destination, false));
        let deadline = Instant::now() + Duration::from_secs(5);
        let connection = loop {
            match silent.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        let started = Instant::now();
        let mut options = Vec::new();
        add_options(&mut options, destination);
        assert!(options.contains(&format!("Port={port}")));
        assert!(!used_options(destination).is_empty());
        // A second preparation is single-flight, including interactive callers.
        prepare(&cancel, destination, true);
        configured_route(worker, port);
        assert!(started.elapsed() < Duration::from_millis(500));
        preparing.join().unwrap();
        drop(connection);
    });
    let mut options = Vec::new();
    add_options(&mut options, destination);
    assert!(
        options.contains(&format!("Port={port}")),
        "obsolete failed preparation replaced the latest route"
    );
}

#[test]
fn isolated_lan_priority_trust_fallback_inline_sign_in_and_no_replay() {
    let _env = env_write();
    if !Path::new("/usr/sbin/sshd").exists() {
        eprintln!("skipping: LAN authentication regression requires OpenSSH sshd");
        return;
    }
    let root = TempDir::new();
    for key in ["host", "impostor", "client"] {
        let output = Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(root.path().join(key))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    fs::copy(root.path().join("host"), root.path().join("lan")).unwrap();
    let user = Command::new("id").arg("-un").output().unwrap();
    assert!(user.status.success());
    let user = String::from_utf8(user.stdout).unwrap();
    let (_good, good_port) = server(root.path(), "host", user.trim());
    let (_lan, lan_port) = server(root.path(), "lan", user.trim());
    let (_bad, bad_port) = server(root.path(), "impostor", user.trim());
    let unavailable = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let unavailable_port = unavailable.local_addr().unwrap().port();
    let directory = root.path().display();
    let host_key = fs::read_to_string(root.path().join("host.pub")).unwrap();
    let known = format!("fixture-original {host_key}");
    fs::write(root.path().join("known_hosts"), &known).unwrap();
    client_config(root.path(), user.trim(), good_port);
    let ssh = root.path().join("ssh");
    fs::write(
        &ssh,
        format!("#!/bin/sh\nexec /usr/bin/ssh -F '{directory}/ssh_config' \"$@\"\n"),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let _path = EnvVar::set("PATH", root.path());
    let _runtime = EnvVar::set("XDG_RUNTIME_DIR", root.path());
    let _locale = EnvVar::set("LC_ALL", "C");
    let _agent = EnvVar::remove("SSH_AUTH_SOCK");
    let _askpass = EnvVar::remove("SSH_ASKPASS");
    let _require = EnvVar::remove("SSH_ASKPASS_REQUIRE");
    let worker = Worker {
        key: "fixture-stable-id".into(),
        name: "fixture".into(),
        destination: Some("delux-lan-fixture".into()),
        online: Some(true),
    };
    let cancel = Cancel::new();
    for (port, selected) in [
        (lan_port, true),
        (unavailable_port, false),
        (bad_port, false),
    ] {
        crate::ssh_mux::begin();
        configured_route(&worker, port);
        let started = Instant::now();
        prepare(&cancel, "delux-lan-fixture", false);
        assert!(started.elapsed() < Duration::from_secs(5));
        let mut chosen = Vec::new();
        add_options(&mut chosen, "delux-lan-fixture");
        assert_eq!(
            !chosen.is_empty(),
            selected,
            "route selection for port {port}: {chosen:?}"
        );
        let marker = root.path().join(format!("mutation-{port}"));
        let script = format!("printf x >> '{}'; exit 17", marker.display());
        let command = crate::app::ssh_command(&worker, &script, false, false);
        let result = run_command(&cancel, &command, Duration::from_secs(5), false).unwrap();
        assert_eq!(result.code, 17, "{result:?}");
        assert_eq!(
            fs::read_to_string(marker).unwrap(),
            "x",
            "mutation replayed"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("known_hosts")).unwrap(),
            known
        );
        crate::ssh_mux::close();
    }
    new_service_tries_other_addresses_without_accepting_unknown_keys(root.path(), lan_port);
    inline_sign_in_retains_lan_for_locked_keys(root.path(), user.trim(), &worker, lan_port);
    preparing_does_not_block_route_readers_or_publish_obsolete_results(&worker, lan_port);
    let rejected_log = fs::read_to_string(root.path().join("impostor.log")).unwrap();
    assert!(
        !rejected_log.contains("Accepted publickey"),
        "credentials authenticated to impostor: {rejected_log}"
    );
    assert!(
        !rejected_log.contains("userauth-request"),
        "authentication sent to impostor: {rejected_log}"
    );
    crate::ssh_mux::close();
    clear();
}

#[test]
fn canceled_browse_does_not_start_a_daemon() {
    let cancel = Cancel::new();
    cancel.cancel();
    assert!(browse(&cancel).unwrap().is_empty());
}
