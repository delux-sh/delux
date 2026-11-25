"""Read-only reporting diagnosis and explicitly confirmed installation.

Sent over the existing authenticated connection; never installed as a daemon.
Only Python's standard library and the computer's existing tmux are required.
"""

import base64
import ctypes
import hashlib
import json
import os
import pwd
import re
import selectors
import shutil
import stat
import subprocess
import sys
import time

MARKER = b"// delux-agent-status adapter. Installed by `delux agents install`; reinstalling replaces this file."
LIMIT = 1024 * 1024
ENV_KEYS = {
    "HOME", "PATH", "OMP_PROFILE", "PI_PROFILE", "PI_CONFIG_DIR",
    "PI_CODING_AGENT_DIR", "OPENCODE_CONFIG_DIR", "XDG_CONFIG_HOME",
    "CLAUDE_CONFIG_DIR", "CODEX_HOME", "GEMINI_CLI_HOME",
}
AGENTS = {"omp", "pi", "opencode", "claude", "codex", "gemini"}
STATUS = re.compile(r"^(working|waiting|blocked|done|error|idle)(?: [0-9]+)?$")
DEADLINE = None


class SetupError(Exception):
    pass


def run(args, env=None):
    timeout = 5 if DEADLINE is None else min(5, DEADLINE - time.monotonic())
    if timeout <= 0:
        raise SetupError("Reporting setup timed out. Inspect again before installing.")
    try:
        child = subprocess.Popen(args, env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    except OSError as error:
        raise SetupError("Cannot run %s: %s" % (args[0], type(error).__name__)) from error
    output = bytearray()
    end = time.monotonic() + timeout
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            while True:
                remaining = end - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise SetupError("%s timed out; inspect before any retry." % os.path.basename(args[0]))
                chunk = os.read(child.stdout.fileno(), min(65536, LIMIT + 1 - len(output)))
                if not chunk:
                    break
                output.extend(chunk)
                if len(output) > LIMIT:
                    raise SetupError("Diagnostic output exceeds the safety limit.")
        try:
            code = child.wait(timeout=max(0.01, end - time.monotonic()))
        except subprocess.TimeoutExpired as error:
            raise SetupError("Command timed out; inspect before any retry.") from error
        if code:
            raise SetupError("%s failed (exit %s). Check access and installed tools." %
                             (os.path.basename(args[0]), code))
        return output.decode("utf-8", errors="strict")
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        child.stdout.close()


def read_limited(path):
    with open(path, "rb") as source:
        data = source.read(LIMIT + 1)
    if len(data) > LIMIT:
        raise SetupError("File exceeds the safety limit: " + path)
    return data


def process_data(pid):
    """Read only a same-account process; never return unrelated environment values."""
    if sys.platform.startswith("linux"):
        root = "/proc/%s" % pid
        if os.stat(root).st_uid != os.getuid():
            raise SetupError("The agent belongs to another account.")
        argv = read_limited(root + "/cmdline").split(b"\0")
        environment = read_limited(root + "/environ").split(b"\0")
        identity = read_limited(root + "/stat").rsplit(b")", 1)[1].split()[19].decode()
        cwd = os.readlink(root + "/cwd")
    elif sys.platform == "darwin":
        owner = run(["ps", "-p", str(pid), "-o", "uid="]).strip()
        if owner != str(os.getuid()):
            raise SetupError("The agent belongs to another account.")
        # KERN_PROCARGS2 contains argc, executable, argv, then NUL-delimited env.
        libc = ctypes.CDLL(None, use_errno=True)
        mib = (ctypes.c_int * 3)(1, 49, pid)
        size = ctypes.c_size_t(LIMIT)
        buffer = ctypes.create_string_buffer(LIMIT)
        if libc.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0):
            raise SetupError("Cannot inspect the running agent's environment.")
        raw = buffer.raw[:size.value]
        argc = int.from_bytes(raw[:4], sys.byteorder)
        if not 0 < argc < 8192:
            raise SetupError("Invalid process arguments.")
        offset = raw.index(b"\0", 4)
        while offset < len(raw) and raw[offset] == 0:
            offset += 1
        entries = raw[offset:].split(b"\0")
        argv, environment = entries[:argc], entries[argc:]
        identity = run(["ps", "-p", str(pid), "-o", "lstart="]).strip()
        # Relative configuration overrides cannot be resolved safely without cwd.
        cwd = None
    else:
        raise SetupError("Automatic process diagnosis supports Linux and macOS.")
    env = {}
    for entry in environment:
        name, separator, value = entry.partition(b"=")
        key = name.decode("utf-8", errors="replace")
        if separator and key in ENV_KEYS:
            env[key] = value.decode("utf-8", errors="strict")
    return [part.decode("utf-8", errors="strict") for part in argv if part], env, identity, cwd


def agent_name(command, argv):
    name = os.path.basename(command).split(" ", 1)[0]
    if name in AGENTS:
        return name
    for value in argv[:3]:
        base = os.path.basename(value)
        if base in AGENTS or base in {agent + ".js" for agent in AGENTS}:
            return base[:-3] if base.endswith(".js") else base
        if "/@oh-my-pi/pi-coding-agent/" in value:
            return "omp"
        if "/@mariozechner/pi-coding-agent/" in value or "/@earendil-works/pi-coding-agent/" in value:
            return "pi"
        if "/@google/gemini-cli/" in value:
            return "gemini"
        if "/@anthropic-ai/claude-code/" in value:
            return "claude"
        if "/@openai/codex/" in value:
            return "codex"
    return None


def find_agent(root_pid, processes):
    pending = [root_pid]
    visited = set()
    failures = False
    while pending and len(visited) < 512:
        next_level = []
        candidates = []
        for pid in pending:
            if pid in visited:
                continue
            visited.add(pid)
            command = processes.get(pid, (0, ""))[1]
            try:
                argv, env, started, cwd = process_data(pid)
                name = agent_name(command, argv)
                if name:
                    candidates.append((pid, name, argv, env, started, cwd))
            except (OSError, ValueError, UnicodeError, SetupError):
                failures = True
            next_level.extend(child for child, data in processes.items() if data[0] == pid)
        if len(candidates) == 1:
            return candidates[0]
        if candidates:
            raise SetupError("Multiple agents share this pane; automatic installation is ambiguous.")
        pending = next_level
    if failures:
        raise SetupError("Could not inspect all pane processes; no installation target was inferred.")
    raise SetupError("No supported running agent identified. A shell or idle session may not need a reporter.")


def profile_name(value):
    value = value.strip()
    if value in ("", "default"):
        return None
    if (not re.fullmatch(r"[a-z0-9][a-z0-9._-]{0,63}", value) or value.endswith(".") or
            re.match(r"^(con|prn|aux|nul|com[0-9]|lpt[0-9])(?:\.|$)", value)):
        raise SetupError("Invalid OMP profile; no installation target was inferred.")
    return value


def argument(argv, flag):
    found = None
    for index, value in enumerate(argv):
        if value == "--":
            break
        if value == flag and index + 1 < len(argv):
            found = argv[index + 1]
        elif value.startswith(flag + "="):
            found = value[len(flag) + 1:]
    return found


def target_path(agent, argv, env, cwd):
    home = env.get("HOME") or pwd.getpwuid(os.getuid()).pw_dir
    if not os.path.isabs(home):
        raise SetupError("The agent's home directory is not absolute.")

    def absolute(value):
        if value == "~" or value.startswith("~/"):
            value = home + value[1:]
        if not os.path.isabs(value):
            if cwd is None:
                raise SetupError("Cannot resolve this agent's relative configuration directory.")
            value = os.path.join(cwd, value)
        return os.path.normpath(value)

    profile = None
    install_env = {key: value for key, value in env.items() if key in ENV_KEYS}
    install_env["HOME"] = home
    if agent == "omp":
        selected = argument(argv, "--profile")
        if selected is None:
            selected = env.get("OMP_PROFILE", env.get("PI_PROFILE", ""))
        profile = profile_name(selected)
        root = os.path.normpath(os.path.join(home, (env.get("PI_CONFIG_DIR") or ".omp").lstrip("/")))
        override = env.get("PI_CODING_AGENT_DIR")
        try:
            inherited = profile_name(env.get("PI_PROFILE", ""))
        except SetupError:
            inherited = None
        if inherited and override == os.path.join(root, "profiles", inherited, "agent"):
            override = None
        directory = (os.path.join(root, "profiles", profile, "agent") if profile else
                     absolute(override or os.path.join(root, "agent")))
        suffix = "extensions/delux-agent-status.ts"
    elif agent == "pi":
        directory = absolute(env.get("PI_CODING_AGENT_DIR") or os.path.join(home, ".pi/agent"))
        suffix = "extensions/delux-agent-status.ts"
    elif agent == "opencode":
        root = absolute(env.get("XDG_CONFIG_HOME") or os.path.join(home, ".config"))
        directory = absolute(env.get("OPENCODE_CONFIG_DIR") or os.path.join(root, "opencode"))
        suffix = "plugins/delux-agent-status.js"
    elif agent == "claude":
        directory = absolute(env.get("CLAUDE_CONFIG_DIR") or os.path.join(home, ".claude"))
        install_env["CLAUDE_CONFIG_DIR"] = directory
        suffix = "settings.json"
    elif agent == "codex":
        directory = absolute(env.get("CODEX_HOME") or os.path.join(home, ".codex"))
        install_env["CODEX_HOME"] = directory
        suffix = "hooks.json"
    else:
        root = absolute(env.get("GEMINI_CLI_HOME") or home)
        directory = os.path.join(root, ".gemini")
        install_env["GEMINI_CLI_HOME"] = root
        suffix = "settings.json"
    if any(value in argv for value in ("--no-extensions", "--trusted-extension")) or any(
            value.startswith("--trusted-extension=") for value in argv):
        raise SetupError("Agent extension discovery is disabled or restricted. Enable the reporter explicitly before restarting.")
    path = os.path.join(directory, suffix)
    if len(path) > 4096 or any(ord(character) < 32 or ord(character) == 127 for character in path):
        raise SetupError("The configuration path cannot be displayed safely; automatic installation is unavailable.")
    return path, profile, install_env


def file_snapshot(path):
    """Never follow a symlink while inspecting an installation target."""
    path = os.path.abspath(path)
    current = os.path.sep
    for part in path.split(os.path.sep)[1:]:
        current = os.path.join(current, part)
        try:
            metadata = os.lstat(current)
        except FileNotFoundError:
            return None, "absent"
        if stat.S_ISLNK(metadata.st_mode):
            raise SetupError("Automatic installation will not follow a symlink: " + current)
        if current != path and not stat.S_ISDIR(metadata.st_mode):
            raise SetupError("Installation parent is not a directory: " + current)
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid() or metadata.st_nlink != 1:
        raise SetupError("Installation target must be a regular, unshared file owned by this account.")
    data = read_limited(path)
    return data, hashlib.sha256(data).hexdigest()


def environment_for_installer(values):
    environment = os.environ.copy()
    for key in ENV_KEYS | {"TMUX", "TMUX_PANE"}:
        environment.pop(key, None)
    environment.update(values)
    if "PATH" not in environment:
        environment["PATH"] = os.defpath
    return environment


def inspect(payloads):
    fields = ["#{session_id}", "#{session_created}", "#{pane_id}", "#{pane_pid}",
              "#{pid}", "#{start_time}", "#{version}", "#{pane_program_state}", "#{@agent_status}"]
    listing = run(["tmux", "list-panes", "-a", "-F", "\t".join(fields)])
    panes = [line.split("\t") for line in listing.splitlines()]
    if len(panes) > 256 or any(len(row) != len(fields) for row in panes):
        raise SetupError("Unexpected or oversized tmux pane inventory.")
    process_listing = run(["ps", "-eo", "pid=,ppid=,comm="])
    processes = {}
    for line in process_listing.splitlines():
        parts = line.strip().split(None, 2)
        if len(parts) == 3 and parts[0].isdigit() and parts[1].isdigit():
            processes[int(parts[0])] = (int(parts[1]), parts[2])
    if len(processes) > 32768:
        raise SetupError("Process inventory exceeds the safety limit.")
    version = panes[0][6] if panes else "unknown"
    native = None
    stable = re.fullmatch(r"(\d+)\.(\d+)(?:[a-z])?", version)
    if stable:
        native = tuple(map(int, stable.groups())) > (3, 8)
    if any(row[7] for row in panes):
        native = True
    targets, session_names, hooks_cache = [], {}, {}
    for row in panes:
        session, created, pane, shell, server_pid, server_started, _, program, option = row
        if not re.fullmatch(r"\$[0-9]+", session) or not re.fullmatch(r"%[0-9]+", pane) or not shell.isdigit():
            raise SetupError("Invalid tmux pane identity.")
        if session not in session_names:
            session_names[session] = run(["tmux", "display-message", "-p", "-t", session + ":", "#{session_name}"]).rstrip("\n")
        identity = [session, created, pane, shell, server_pid, server_started]
        target = {"id": ":".join(identity), "sessionId": session, "sessionName": session_names[session],
                  "paneId": pane, "agent": "unknown", "profile": None, "state": "unknown",
                  "detail": "No current status.", "files": [], "token": None, "action": None}
        observed = program if program else option
        has_status = bool(STATUS.fullmatch(observed)) and observed.split(" ", 1)[0] != "idle"
        try:
            pid, agent, argv, env, started, cwd = find_agent(int(shell), processes)
            target["agent"] = agent
            target["id"] += ":%s:%s" % (pid, started)
            path, profile, installer_env = target_path(agent, argv, env, cwd)
            target["profile"] = profile
            data, digest = file_snapshot(path)
            files = [path]
            action = "install"
            installer = None
            adapter = payloads.get("pi" if agent in ("omp", "pi") else agent)
            if adapter is not None:
                if data is not None and not data.startswith(MARKER + b"\n"):
                    raise SetupError("An unowned file already occupies the reporter path; it will not be overwritten: " + path)
                if data == adapter.encode():
                    target.update(state="installed", detail="Reporter installed, but no current status observed. Restart/resume at a safe point and check that the extension is enabled.", files=files)
                    action = None
                elif data is not None:
                    target.update(state="outdated", detail="A Delux-owned reporter update is available.")
                    action = "update"
                else:
                    target.update(state="missing", detail="No reporter installed for this agent/profile.")
            else:
                install_environment = environment_for_installer(installer_env)
                executable = shutil.which("delux", path=install_environment.get("PATH"))
                key = (agent, path, executable)
                if key not in hooks_cache:
                    try:
                        if not executable:
                            raise SetupError("Delux is not installed on this computer.")
                        text = run([executable, "agents", "status", agent], env=install_environment)
                        if "reporting installed" in text:
                            hooks_cache[key] = (executable, "installed")
                        elif "reporting not installed" in text or "reporting partial" in text:
                            hooks_cache[key] = (executable, "missing")
                        else:
                            raise SetupError("Installed Delux cannot diagnose these hooks.")
                    except SetupError:
                        hooks_cache[key] = (None, "unknown")
                installer, state = hooks_cache[key]
                target.update(state=state, detail="Hook reporter installed; restart the agent at a safe point. Codex also requires trusting hooks with /hooks.")
                if state == "installed":
                    action = None
                elif not installer:
                    action = None
                    target.update(state="missing" if data is None else "unknown", detail="Install/update Delux on this computer, then run `delux agents install %s`. Automatic setup cannot inspect the existing hook configuration yet." % agent)
                else:
                    target.update(state="missing", detail="Install Delux's agent hooks into this configuration. Existing unrelated hooks are retained; Codex requires /hooks trust afterward.")
                if data is not None:
                    _, backup_digest = file_snapshot(path + ".delux-backup")
                    if backup_digest == "absent":
                        files.append(path + ".delux-backup")
            if has_status:
                action = None
                target.update(state="reporting", detail="A current agent status is present in tmux; reporting has been observed.")
            elif native is True and data is None and agent in ("omp", "codex"):
                action = None
                target.update(state="native", detail="This tmux server supports native reporting and this agent emits it. No current status observed; an adapter is not required for native reporting.")
            target["files"] = files
            target["action"] = action
            if action:
                snapshot = {"identity": identity, "pid": pid, "started": started, "agent": agent,
                            "profile": profile, "path": path, "digest": digest, "files": files,
                            "installer": installer, "environment": installer_env,
                            "payload": hashlib.sha256((adapter or "").encode()).hexdigest()}
                target["token"] = base64.urlsafe_b64encode(json.dumps(snapshot, sort_keys=True, separators=(",", ":")).encode()).decode()
        except (OSError, ValueError, UnicodeError, SetupError) as error:
            target.update(state="reporting" if has_status else "unknown", detail=("Agent status observed. " if has_status else "") + str(error), action=None, token=None)
            if "symlink" in str(error) or "unowned" in str(error) or "regular, unshared" in str(error):
                target["state"] = "conflict"
        targets.append(target)
    return {"version": 1, "tmuxVersion": version, "native": native, "targets": targets,
            "detail": "Inspection does not change settings or running sessions. Installed is not the same as reporting."}


def install_adapter(path, data, expected_digest):
    """Descriptor-relative writes prevent replacing files through symlink parents."""
    parent, name = os.path.split(path)
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    directory = os.open(os.path.sep, flags)
    temporary = None
    try:
        for part in parent.split(os.path.sep)[1:]:
            if not part or part in (".", ".."):
                raise SetupError("Invalid installation path.")
            try:
                child = os.open(part, flags, dir_fd=directory)
            except FileNotFoundError:
                os.mkdir(part, 0o700, dir_fd=directory)
                child = os.open(part, flags, dir_fd=directory)
            os.close(directory)
            directory = child
        if os.fstat(directory).st_uid != os.getuid():
            raise SetupError("Reporter directory must belong to this account.")
        try:
            current_fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
        except FileNotFoundError:
            current = None
        else:
            with os.fdopen(current_fd, "rb") as source:
                metadata = os.fstat(source.fileno())
                if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid() or metadata.st_nlink != 1:
                    raise SetupError("Reporter file changed ownership or type; inspect again.")
                current = source.read(LIMIT + 1)
        digest = "absent" if current is None else hashlib.sha256(current).hexdigest()
        if digest != expected_digest:
            raise SetupError("Reporter changed after inspection; inspect again before installing.")
        if current is not None and not current.startswith(MARKER + b"\n"):
            raise SetupError("Refusing to replace an unowned reporter file.")
        temporary = ".delux-reporting-" + os.urandom(12).hex()
        descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=directory)
        with os.fdopen(descriptor, "wb") as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        # A missing destination uses link()'s no-clobber semantics.
        if current is None:
            os.link(temporary, name, src_dir_fd=directory, dst_dir_fd=directory, follow_symlinks=False)
            os.unlink(temporary, dir_fd=directory)
        else:
            # Serialize cooperative updater instances and verify once more before replacement.
            import fcntl
            lock_fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                if os.fstat(lock_fd).st_ino != os.stat(name, dir_fd=directory, follow_symlinks=False).st_ino:
                    raise SetupError("Reporter changed concurrently; inspect again.")
                with os.fdopen(os.dup(lock_fd), "rb") as locked:
                    if hashlib.sha256(locked.read(LIMIT + 1)).hexdigest() != expected_digest:
                        raise SetupError("Reporter changed concurrently; inspect again.")
                os.replace(temporary, name, src_dir_fd=directory, dst_dir_fd=directory)
            finally:
                os.close(lock_fd)
        temporary = None
        os.fsync(directory)
    finally:
        if temporary is not None:
            os.unlink(temporary, dir_fd=directory)
        os.close(directory)


def install(payloads, token):
    if not isinstance(token, str) or len(token) > 65536:
        raise SetupError("Invalid installation token.")
    current = inspect(payloads)
    match = next((target for target in current["targets"] if target["token"] == token and target["action"]), None)
    if match is None:
        raise SetupError("Agent, profile, or files changed since inspection. Inspect again; nothing was installed.")
    snapshot = json.loads(base64.urlsafe_b64decode(token))
    if snapshot["installer"]:
        run([snapshot["installer"], "agents", "install", snapshot["agent"]],
            env=environment_for_installer(snapshot["environment"]))
        verified = inspect(payloads)
        if not any(target["id"] == match["id"] and target["state"] in ("installed", "reporting") for target in verified["targets"]):
            raise SetupError("Hook installation was not confirmed. Inspect before any retry; the command may have changed files.")
    else:
        adapter = payloads["pi" if snapshot["agent"] in ("omp", "pi") else snapshot["agent"]]
        install_adapter(snapshot["path"], adapter.encode(), snapshot["digest"])
    detail = "Installed. Restart/resume the agent at a safe point; already-open questions are not replayed. Reporting is confirmed only when an actual status arrives."
    if snapshot["agent"] == "codex":
        detail += " Codex hooks also require /hooks trust."
    return {"version": 1, "status": "installed", "files": match["files"], "detail": detail}


def main():
    global DEADLINE
    DEADLINE = time.monotonic() + 15
    try:
        if len(sys.argv) not in (3, 4) or sys.argv[1] not in ("inspect", "install"):
            raise SetupError("Expected inspect PAYLOADS or install PAYLOADS TOKEN.")
        payloads = json.loads(sys.argv[2])
        if not isinstance(payloads, dict) or set(payloads) != {"pi", "opencode"} or any(
                not isinstance(value, str) or len(value) > 65536 or not value.startswith(MARKER.decode() + "\n")
                for value in payloads.values()):
            raise SetupError("Invalid packaged reporter payloads.")
        if sys.argv[1] == "inspect" and len(sys.argv) == 3:
            result = inspect(payloads)
        elif sys.argv[1] == "install" and len(sys.argv) == 4:
            result = install(payloads, sys.argv[3])
        else:
            raise SetupError("Invalid reporting setup operation.")
        print(json.dumps(result, ensure_ascii=True, separators=(",", ":")))
    except (OSError, ValueError, UnicodeError, SetupError) as error:
        print(json.dumps({"version": 1, "error": str(error)}, ensure_ascii=True, separators=(",", ":")))
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
