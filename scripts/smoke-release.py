#!/usr/bin/env python3
"""Exercise an installed Delux picker in a real PTY and a private default tmux server.

Usage: python3 scripts/smoke-release.py /absolute/path/to/delux [--version VERSION]
Only the Python standard library and the installed tmux dependency are required.
"""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time


class Picker:
    def __init__(self, executable, env, home):
        self.pid = None
        self.fd = None
        self.status = None
        self.output = bytearray()
        self.pending = bytearray()
        pid, fd = pty.fork()
        if pid == 0:
            try:
                os.chdir(home)
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
                os.execve(executable, [executable, "--local"], env)
            except BaseException as error:
                os.write(2, f"picker exec failed: {error}\n".encode())
                os._exit(127)
        self.pid, self.fd = pid, fd

    def poll(self):
        if self.status is None:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = os.waitstatus_to_exitcode(status)
        return self.status

    def send(self, data):
        if self.poll() is not None:
            raise RuntimeError(f"picker exited early: {self.status}")
        while data:
            data = data[os.write(self.fd, data):]

    def pump(self, timeout=0.05):
        if not select.select([self.fd], [], [], timeout)[0]:
            return
        try:
            data = os.read(self.fd, 65536)
        except OSError as error:
            if error.errno == errno.EIO:  # Linux signals the closed slave this way.
                return
            raise
        self.output.extend(data)
        del self.output[:-131072]
        self.pending.extend(data)
        # Answer actual terminal capability queries, including queries split
        # across reads. Do not infer readiness from a timer or UI help wording.
        replies = {
            b"\x1b[6n": b"\x1b[1;1R",
            b"\x1b[c": b"\x1b[?1;2c",
            b"\x1b[0c": b"\x1b[?1;2c",
            b"\x1b[>c": b"\x1b[>0;0;0c",
            b"\x1b[>0c": b"\x1b[>0;0;0c",
            b"\x1b[?u": b"\x1b[?0u",
            b"\x1b[?2026$p": b"\x1b[?2026;2$y",
        }
        consumed = 0
        for match in re.finditer(rb"\x1b\[[0-?]*[ -/]*[@-~]", self.pending):
            consumed = match.end()
            reply = replies.get(match.group())
            if reply is not None and self.poll() is None:
                self.send(reply)
        del self.pending[:consumed]
        del self.pending[:-256]

    def wait(self, description, condition, exiting=False):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            self.pump()
            if condition():
                return
            if not exiting and self.poll() is not None:
                raise RuntimeError(f"picker exited during {description}: {self.status}")
        raise RuntimeError(f"timed out waiting for {description}; PTY tail: {bytes(self.output[-2000:])!r}")

    def close(self):
        try:
            if self.pid is not None and self.poll() is None:
                # Signal only the child we forked, never an inferred group ID
                # (especially not group 0 after a failed spawn).
                os.kill(self.pid, signal.SIGTERM)
                deadline = time.monotonic() + 3
                while self.poll() is None and time.monotonic() < deadline:
                    self.pump()
                if self.poll() is None:
                    os.kill(self.pid, signal.SIGKILL)
                    os.waitpid(self.pid, 0)
        finally:
            if self.fd is not None:
                os.close(self.fd)
                self.fd = None


def smoke(executable, version, root):
    tmux = shutil.which("tmux")
    if tmux is None:
        raise RuntimeError("the installed tmux dependency is not on PATH")
    home = root / "home"
    home.mkdir()
    config = root / "config"
    state = root / "state"
    config.mkdir()
    state.mkdir()
    # Deliberate allowlist: no inherited TMUX, TMUX_PANE, SSH agent, shell rc
    # variables, user configuration, or discovery credentials. Keep the brew
    # bin directory so Delux resolves the same installed dependencies.
    env = {
        "PATH": f"{Path(tmux).parent}:/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": str(home),
        "XDG_CONFIG_HOME": str(config),
        "XDG_STATE_HOME": str(state),
        "XDG_CACHE_HOME": str(root / "cache"),
        "TMUX_TMPDIR": str(root),
        "TMPDIR": str(root),
        "SHELL": "/bin/sh",
        "TERM": "xterm-256color",
        "LC_ALL": "C",
    }
    socket = root / f"tmux-{os.getuid()}" / "default"
    tmux_config = root / "tmux.conf"
    tmux_config.write_text('set -g default-shell /bin/sh\nset -g default-command /bin/sh\nset -g update-environment ""\n')

    def run(*args, check=True):
        return subprocess.run(args, env=env, cwd=home, capture_output=True,
                              text=True, timeout=5, check=check)

    def control(*args, check=True):
        # Every control/cleanup command names the private socket explicitly.
        return run(tmux, "-N", "-S", str(socket), *args, check=check)

    def inventory():
        data = json.loads(run(executable, "list", "--local", "--json").stdout)
        return [session for computer in data["computers"] for session in computer["sessions"]]

    def attached():
        return control("display-message", "-p", "-t", "release-smoke", "#{session_attached}").stdout.strip()

    def pane_identity():
        return control("display-message", "-p", "-t", "release-smoke",
                       "#{pid}:#{session_id}:#{pane_id}:#{pane_pid}:#{pane_dead}").stdout.strip()

    picker = None
    try:
        reported_version = run(executable, "--version").stdout.strip()
        if version is not None and reported_version != f"delux {version}":
            raise RuntimeError(f"wrong installed version: {reported_version!r}")
        if inventory():
            raise RuntimeError("private default socket unexpectedly contains sessions before startup")
        # -L default is intentional: Delux discovers the default server. The
        # fresh, short TMUX_TMPDIR isolates it without changing that behavior.
        run(tmux, "-f", str(tmux_config), "-L", "default", "new-session",
            "-d", "-s", "release-smoke", "-x", "120", "-y", "40", "/bin/sh")
        sessions = inventory()
        if len(sessions) != 1 or sessions[0]["name"] != "release-smoke":
            raise RuntimeError(f"installed CLI did not discover the fixture: {sessions!r}")
        original = sessions[0]
        if Path(original["socket_path"]).resolve() != socket.resolve():
            raise RuntimeError("installed CLI discovered a socket outside the fixture")
        identity = pane_identity()
        if not identity.endswith(":0") or attached() != "0":
            raise RuntimeError(f"fixture pane is not live and detached: {identity}")

        for phase in ("focus", "detach"):
            picker = Picker(executable, env, home)
            picker.wait("fixture rendering and raw terminal mode", lambda:
                        b"release-smoke" in picker.output and
                        not (termios.tcgetattr(picker.fd)[3] & termios.ICANON))
            # End selects the sole session under its computer row; Enter
            # opens the actual embedded tmux client, not a source-built proxy.
            picker.send(b"\x1b[F\r")
            picker.wait("embedded tmux attachment", lambda: attached() == "1")
            marker = root / phase
            command = f"printf '%s' {phase} > {shlex.quote(str(marker))}\r"
            picker.send(command.encode())
            picker.wait("input reaching the session shell", lambda:
                        marker.exists() and marker.read_text() == phase)
            if phase == "focus":
                # q must quit the picker, not reach the shell. This proves
                # Shift-Tab returns focus without first detaching the client.
                picker.send(b"\x1b[Zq")
            else:
                picker.send(b"\x02d")  # tmux's native prefix + detach binding.
                picker.wait("native tmux detachment", lambda: attached() == "0")
                if picker.poll() is not None:
                    raise RuntimeError("detaching tmux unexpectedly closed the picker")
                # The server drops its client before the picker necessarily
                # processes that exit. Return focus explicitly so q cannot be
                # sent to the just-closed embedded PTY in that interval.
                picker.send(b"\x1b[Zq")
            picker.wait("clean picker exit", lambda: picker.poll() is not None, exiting=True)
            if picker.status != 0:
                raise RuntimeError(f"picker returned {picker.status}; PTY tail: {bytes(picker.output[-2000:])!r}")
            picker.wait("client cleanup after picker exit", lambda: attached() == "0", exiting=True)
            picker.close()
            picker = None
            if attached() != "0" or pane_identity() != identity:
                raise RuntimeError("picker changed or destroyed the existing session/pane")
            remaining = inventory()
            stable = ("id", "name", "windows", "server_pid", "server_start", "socket_path", "created")
            if len(remaining) != 1 or any(remaining[0][key] != original[key] for key in stable):
                raise RuntimeError(f"session identity was not preserved: {remaining!r}")
        print(f"{reported_version}: installed inventory, picker attach/input/focus/detach, and session preservation passed")
    finally:
        try:
            if picker is not None:
                picker.close()
        finally:
            control("kill-server", check=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path)
    parser.add_argument("--version")
    args = parser.parse_args()
    if not args.executable.is_absolute() or not args.executable.is_file() or not os.access(args.executable, os.X_OK):
        parser.error("executable must be an absolute path to an installed executable")

    def interrupted(signum, _frame):
        raise RuntimeError(f"smoke test interrupted by signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGALRM, interrupted)
    signal.alarm(180)
    try:
        # macOS's normal TMPDIR is too long for portable Unix socket paths.
        # mkdtemp creates a private mode-0700 directory, never a shared server.
        with tempfile.TemporaryDirectory(prefix="dlx-", dir="/tmp") as directory:
            smoke(str(args.executable.resolve()), args.version, Path(directory))
    finally:
        signal.alarm(0)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        print(f"release smoke failed: {error}", file=sys.stderr)
        sys.exit(1)
