#!/usr/bin/env python3
"""Record the README demo as an asciicast, using only synthetic data.

Usage: python3 scripts/record-demo.py /absolute/path/to/delux docs/demo.cast

Starts a private loopback sshd and three isolated tmux servers ("local",
"devbox" and "build-server") with staged output, then drives the real picker
in a PTY. It never reads your SSH configuration or agent, your tmux sessions,
or Tailscale, and removes everything it created on exit. Needs only the Python
standard library, tmux, make, and OpenSSH's ssh, ssh-keygen and /usr/sbin/sshd.

Render the cast with agg (https://github.com/asciinema/agg):

    agg --theme github-dark --font-size 14 --fps-cap 15 \
        --last-frame-duration 3 docs/demo.cast docs/demo.gif
"""

import codecs
import fcntl
import getpass
import json
import os
from pathlib import Path
import pty
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

COLS, ROWS = 104, 28

AGENT = r"""#!/bin/sh
# Staged output for the README demo; not a real agent.
while :; do
  for task in 'Add retry with backoff to the upload client' \
      'Reject stale session IDs' 'Rotate auth tokens on refresh'; do
    printf '\033[1magent\033[0m \033[2m- api - staged demo output\033[0m\n\n'
    printf '\033[36m>\033[0m %s\n\n' "$task"; sleep 2
    printf '\033[33m*\033[0m Reading the code\n'; sleep 1
    printf '\033[33m*\033[0m Editing 2 files \033[2m(+24 -6)\033[0m\n'; sleep 1
    printf '\033[33m*\033[0m Running make test\n'; sleep 2
    printf '  \033[32m6 passed\033[0m, 0 failed\n\n'; sleep 1
    printf '\033[32mDone.\033[0m \033[2mWaiting for your next instruction...\033[0m\n\n'; sleep 6
  done
done
"""

BUILD = r"""#!/bin/sh
# Staged output for the README demo.
i=1
while :; do
  for crate in lexer parser codegen runtime cli; do
    printf '\033[32m   Compiling\033[0m %s v0.%s.0\n' "$crate" "$i"; sleep 0.6
  done
  printf '\033[32m    Finished\033[0m release profile in 4.2s\n'
  printf '\033[36m   Packaging\033[0m app-0.%s.0.tar.gz\n\n' "$i"; sleep 3
  i=$((i + 1))
done
"""

LOGS = r"""#!/bin/sh
# Staged output for the README demo.
n=0
while :; do
  for path in /health /api/uploads /api/sessions /assets/app.js; do
    n=$((n + 1))
    printf '%s \033[32m200\033[0m GET %s %sms\n' "$(date +%H:%M:%S)" "$path" "$((n % 37 + 3))"
    sleep 0.8
  done
done
"""

TESTS = r"""#!/bin/sh
# Staged output for the README demo.
printf 'running 6 tests\n'
for name in upload::retries_with_backoff upload::gives_up_after_three \
    sessions::lists_by_activity sessions::rejects_stale_id \
    auth::rotates_tokens health::reports_ready; do
  sleep 0.15
  printf 'test %s ... \033[32mok\033[0m\n' "$name"
done
printf '\ntest result: \033[32mok\033[0m. 6 passed; 0 failed\n'
"""

# computer -> (session, project directory, staged program or None for a shell),
# in display order. The picker lists each computer's newest session first.
SESSIONS = {
    "local": [("notes", "notes", None), ("web-app", "web-app", "demo-logs")],
    "devbox": [("api", "api", None), ("review-agent", "api", "demo-agent")],
    "build-server": [("deploy-logs", "infra", "demo-logs"), ("release-build", "app", "demo-build")],
}


def executable(path, text):
    path.write_text(text)
    path.chmod(0o755)


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


class Demo:
    def __init__(self, root):
        self.root = root
        self.tmux = shutil.which("tmux")
        if not self.tmux:
            raise RuntimeError("tmux is required")
        self.path = f"{root}/bin:{Path(self.tmux).parent}:/usr/bin:/bin"
        self.sshd = None
        for name in ("bin", "ssh", "config", "state", "home"):
            (root / name).mkdir()
        (root / "run").mkdir(mode=0o700)
        for name, text in (("agent", AGENT), ("build", BUILD), ("logs", LOGS), ("tests", TESTS)):
            executable(root / "bin" / f"demo-{name}", text)

    def environment(self, computer):
        return {
            "PATH": self.path,
            "HOME": str(self.root / "home"),
            "TERM": "xterm-256color",
            "LANG": "en_US.UTF-8",
            "SHELL": "/bin/sh",
            "TMUX_TMPDIR": str(self.root / "tmux" / computer),
            "BASH_SILENCE_DEPRECATION_WARNING": "1",
        }

    def control(self, computer, *args, check=True):
        return subprocess.run(
            [self.tmux, *args],
            env=self.environment(computer),
            check=check,
            capture_output=True,
            text=True,
        )

    def start_sessions(self):
        for computer, sessions in SESSIONS.items():
            (self.root / "tmux" / computer).mkdir(parents=True)
            config = self.root / "config" / f"{computer}.tmux.conf"
            config.write_text(
                'set -g status-left "[#S] "\nset -g status-left-length 30\n'
                f'set -g status-right " {computer} "\nset -g escape-time 0\n'
            )
            for index, (session, project, program) in enumerate(reversed(sessions)):
                if index:
                    time.sleep(1.1)  # distinct creation seconds fix the display order
                directory = self.root / "home" / computer / project
                directory.mkdir(parents=True, exist_ok=True)
                (directory / "Makefile").write_text("test:\n\t@demo-tests\n")
                prompt = rf"\[\033[32m\]alex@{computer}\[\033[0m\]:\[\033[34m\]{project}\[\033[0m\]$ "
                command = program or f"env PS1='{prompt}' /bin/bash --norc --noprofile"
                self.control(
                    computer, "-f", str(config), "new-session", "-d", "-s", session,
                    "-x", "70", "-y", "24", "-c", str(directory), command,
                )

    def start_ssh(self):
        ssh = self.root / "ssh"
        for key in ("host", "client"):
            subprocess.run(
                ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "delux-demo", "-f", str(ssh / key)],
                check=True,
            )
        host_key = " ".join((ssh / "host.pub").read_text().split()[:2])
        user = getpass.getuser()
        remotes = [computer for computer in SESSIONS if computer != "local"]
        ports = {computer: free_port() for computer in remotes}
        server = [f"Port {port}" for port in ports.values()]
        server += [
            "ListenAddress 127.0.0.1",
            f"HostKey {ssh / 'host'}",
            f"PidFile {ssh / 'sshd.pid'}",
            f"AuthorizedKeysFile {ssh / 'client.pub'}",
            "StrictModes no",
            "PasswordAuthentication no",
            "KbdInteractiveAuthentication no",
            "UsePAM no",
            f"AllowUsers {user}",
            "LogLevel ERROR",
        ]
        client, known = [], []
        for index, (computer, port) in enumerate(ports.items()):
            remote = self.root / "bin" / f"remote-{computer}"
            exports = " ".join(f"{key}='{value}'" for key, value in self.environment(computer).items())
            # Computers are listed as they respond. Answer each one's first
            # probe a little later than the last so the order never varies.
            seen = ssh / f"{computer}.seen"
            late = f"[ -e '{seen}' ] || {{ : > '{seen}'; sleep {index * 0.5}; }}\n"
            executable(
                remote,
                f"#!/bin/sh\nunset TMUX TMUX_PANE SSH_AUTH_SOCK\nexport {exports}\n{late}"
                'exec /bin/sh -c "$SSH_ORIGINAL_COMMAND"\n',
            )
            server += [f"Match LocalPort {port}", f"    ForceCommand {remote}"]
            client += [f"Host {computer}", "    HostName 127.0.0.1", f"    Port {port}"]
            known.append(f"[127.0.0.1]:{port} {host_key}")
        client += [
            "Host *",
            f"    User {user}",
            f"    IdentityFile {ssh / 'client'}",
            "    IdentitiesOnly yes",
            "    IdentityAgent none",
            f"    UserKnownHostsFile {ssh / 'known_hosts'}",
            "    GlobalKnownHostsFile /dev/null",
            "    LogLevel ERROR",
        ]
        (ssh / "sshd_config").write_text("\n".join(server) + "\n")
        (ssh / "ssh_config").write_text("\n".join(client) + "\n")
        (ssh / "known_hosts").write_text("\n".join(known) + "\n")
        executable(self.root / "bin" / "ssh", f"#!/bin/sh\nexec /usr/bin/ssh -F '{ssh / 'ssh_config'}' \"$@\"\n")
        self.sshd = subprocess.Popen(
            ["/usr/sbin/sshd", "-D", "-e", "-f", str(ssh / "sshd_config")],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + 10
        for port in ports.values():
            while True:
                try:
                    socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
                    break
                except OSError:
                    if self.sshd.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError("the demo sshd did not start")
                    time.sleep(0.05)
        return remotes

    def stop(self):
        for computer in SESSIONS:
            self.control(computer, "kill-server", check=False)
        if self.sshd and self.sshd.poll() is None:
            self.sshd.terminate()
            self.sshd.wait(timeout=5)


class Recorder:
    """Runs the picker in a PTY and timestamps its output as asciicast events."""

    def __init__(self, arguments, env, cwd):
        self.events = []
        self.recording = True
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            try:
                os.chdir(cwd)
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
                os.execve(arguments[0], arguments, env)
            finally:
                os._exit(127)
        self.started = time.monotonic()
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()

    def read(self):
        decoder = codecs.getincrementaldecoder("utf-8")("replace")
        while True:
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            if not data:
                return
            text = decoder.decode(data)
            if text and self.recording:
                self.events.append([round(time.monotonic() - self.started, 3), "o", text])

    def key(self, data, pause):
        os.write(self.fd, data)
        time.sleep(pause)

    def type(self, text, pause):
        for character in text:
            self.key(character.encode(), 0.07)
        time.sleep(pause)

    def finish(self):
        self.recording = False
        self.key(b"\x1b[Z", 0.2)
        self.key(b"q", 0.2)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if os.waitpid(self.pid, os.WNOHANG)[0]:
                return
            time.sleep(0.05)
        os.kill(self.pid, 9)
        os.waitpid(self.pid, 0)


def walkthrough(picker):
    wheel = f"\x1b[<65;{COLS - 30};12M".encode()
    picker.key(b"", 2.4)  # sessions stream in as computers respond
    for _ in range(3):  # browse; the preview follows the selection
        picker.key(b"j", 1.5)
    picker.key(b"/", 0.4)  # filter for a project
    picker.type("api", 0.9)
    picker.key(b"\r", 0.7)
    picker.key(b"\r", 1.6)  # open the session in the right-hand panel
    picker.type("make test", 0.3)
    picker.key(b"\r", 2.0)
    picker.key(b"\x1b[Z", 0.9)  # back to the list; the session stays connected
    picker.key(b"\x1b", 0.9)  # clear the filter; the opened session stays selected
    picker.key(b"j", 1.6)  # browse another session without disconnecting
    picker.key(b"f", 1.4)  # pin it
    picker.key(wheel, 0.1)  # scrolling an inactive panel explains itself
    picker.key(wheel, 2.2)
    picker.key(b"\t", 1.6)  # return to the connected terminal


def main():
    if len(sys.argv) != 3 or not Path(sys.argv[1]).is_absolute():
        sys.exit(__doc__)
    binary, output = sys.argv[1], Path(sys.argv[2])
    # A short root keeps tmux and OpenSSH sockets within Unix path limits.
    root = Path(tempfile.mkdtemp(prefix="dxd-", dir="/tmp"))
    demo = Demo(root)
    try:
        demo.start_sessions()
        remotes = demo.start_ssh()
        env = demo.environment("local")
        env.update(
            XDG_CONFIG_HOME=str(root / "config"),
            XDG_STATE_HOME=str(root / "state"),
            XDG_RUNTIME_DIR=str(root / "run"),
        )
        arguments = [binary]
        for computer in remotes:
            arguments += ["--host", computer]
        picker = Recorder(arguments, env, root / "home")
        try:
            walkthrough(picker)
        finally:
            picker.finish()
        header = {"version": 2, "width": COLS, "height": ROWS, "env": {"TERM": "xterm-256color"}}
        with output.open("w") as cast:
            for line in [header, *picker.events]:
                cast.write(json.dumps(line, ensure_ascii=False) + "\n")
        print(f"wrote {output}: {len(picker.events)} events, {picker.events[-1][0]:.1f}s")
    finally:
        demo.stop()
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
