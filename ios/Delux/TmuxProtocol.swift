import Foundation

// DELUX_SESSIONS_4 and its base64 field framing match src/tmux.rs.
// Every dynamic shell argument is quoted; pane option bytes are filtered before
// list-panes adds newlines, so a malicious option cannot inject another pane.
enum TmuxProtocol {
    static func quote(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\"'\"'") + "'"
    }

    static func shell(_ script: String) -> String { "sh -c " + quote(script) }

    static func validID(_ id: String) -> Bool {
        id.first == "$" && digits(String(id.dropFirst()))
    }

    private static func digits(_ value: String) -> Bool {
        !value.isEmpty && value.utf8.allSatisfy { (48...57).contains($0) }
    }

    static let prelude = #"""
    unset TMUX
    set -f
    command -v tmux >/dev/null 2>&1 || { printf '%s\n' 'Install tmux 3.2 or newer on this computer.' >&2; exit 70; }
    command tmux -N -V >/dev/null 2>&1 || { printf '%s\n' 'Delux requires tmux 3.2 or newer.' >&2; exit 71; }
    tmux_cmd() { command tmux -u -N -L default "$@"; }
    """# + "\n"

    private static let encodingPrelude = #"""
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
    """# + "\n"

    static let inventory = prelude + encodingPrelude + #"""
    session_times() {
        rows=$(tmux_cmd list-sessions -F '#{session_id} #{pid} #{start_time} #{session_created} #{session_activity}') || return 1
        while IFS= read -r row; do
            case "$row" in "$1 "*) printf '%s\n' "$row"; return 0 ;; esac
        done <<EOF
    $rows
    EOF
        return 1
    }
    agent_status_format='#{?pane_program_state,#{pane_program_state},#{?#{m/r:[^a-z0-9 ],#{@agent_status}},,#{@agent_status}}}'
    emit() {
        encode "$1" || exit 72
        printf '%b' "$2"
    }
    field() {
        value=$(tmux_cmd display-message -p -t "$1" "$2" && printf '.') || exit 73
        value=${value%.}
        value=${value%?}
        emit "$value" "$3"
    }
    sessions=$(tmux_cmd list-sessions -F '#{session_id}' 2>&1)
    status=$?
    if [ "$status" -ne 0 ]; then
        case "$status:$sessions" in
            '1:no server running on '*|'1:error connecting to '*' (No such file or directory)')
                printf '%s\n' 'DELUX_SESSIONS_4' 'DELUX_END'
                exit 0 ;;
            *) printf '%s\n' "$sessions" >&2; exit 73 ;;
        esac
    fi
    printf '%s\n' 'DELUX_SESSIONS_4'
    for session in $sessions; do
        case "$session" in '$'*) digits=${session#?} ;; *) exit 73 ;; esac
        case "$digits" in ''|*[!0-9]*) exit 73 ;; esac
        target="$session:"
        before=$(session_times "$session") || exit 73
        set -- $before
        [ "$#" -eq 5 ] && [ "$1" = "$session" ] || exit 73
        before="$1 $2 $3 $4"
        server_pid=$2
        server_start=$3
        created=$4
        activity=$5
        emit "$session" '\t'
        field "$target" '#{session_name}' '\t'
        field "$target" '#{session_windows}' '\t'
        field "$target" '#{session_attached}' '\t'
        emit "$server_pid" '\t'
        emit "$server_start" '\t'
        field "$target" '#{socket_path}' '\t'
        emit "$created" '\t'
        emit "$activity" '\t'
        window_output=$(tmux_cmd list-windows -t "$target" -F '#{window_activity}') || exit 73
        output=0
        for value in $window_output; do
            case "$value" in ''|*[!0-9]*) exit 73 ;; esac
            [ "$value" -gt "$output" ] && output=$value
        done
        emit "$output" '\t'
        field "$target" '#{pane_current_command}' '\t'
        field "$target" '#{pane_current_path}' '\t'
        pane_status=$(tmux_cmd list-panes -s -t "$target" -F "$agent_status_format") || exit 73
        emit "$pane_status" '\n'
        after=$(session_times "$session") || exit 73
        set -- $after
        [ "$#" -eq 5 ] && [ "$before" = "$1 $2 $3 $4" ] || exit 73
    done
    printf '%s\n' 'DELUX_END'
    """#

    static func parseInventory(_ data: Data, now: Int64 = Int64(Date().timeIntervalSince1970)) throws -> [RemoteSession] {
        guard var text = String(data: data, encoding: .utf8) else { throw malformed() }
        if text.hasSuffix("\n") { text.removeLast() }
        let records = text.components(separatedBy: "\n")
        guard records.count >= 2, records.first == "DELUX_SESSIONS_4", records.last == "DELUX_END" else {
            throw malformed()
        }
        var seen = Set<String>()
        return try records.dropFirst().dropLast().map { record in
            let encoded = record.components(separatedBy: "\t")
            guard encoded.count == 13 else { throw malformed() }
            let fields = try encoded.map { value -> String in
                guard let bytes = Data(base64Encoded: value), bytes.base64EncodedString() == value,
                      let decoded = String(data: bytes, encoding: .utf8), !decoded.contains("\0") else {
                    throw malformed()
                }
                return decoded
            }
            guard validID(fields[0]), seen.insert(fields[0]).inserted,
                  let windows = Int(fields[2]), windows > 0, !fields[6].isEmpty else { throw malformed() }
            for index in [2, 3, 4, 5, 7, 8, 9] {
                guard digits(fields[index]), let value = Int64(fields[index]), value >= (index == 3 ? 0 : 1) else {
                    throw malformed()
                }
            }
            let output = Int64(fields[9])!
            var winner: (status: AgentStatus, since: Int64?)?
            for pane in fields[12].components(separatedBy: "\n") {
                guard let next = parseStatus(pane) else { continue }
                if winner == nil || next.status.priority < winner!.status.priority ||
                    (next.status == winner!.status && (next.since ?? 0) > (winner!.since ?? 0)) {
                    winner = next
                }
            }
            // Match the CLI's stale Working rule; waiting/error/done never expire.
            if let agent = winner, agent.status == .working, now - max(agent.since ?? 0, output) > 600 {
                winner = nil
            }
            return RemoteSession(id: fields[0], name: fields[1], windows: windows,
                                 serverPID: Int64(fields[4])!, serverStarted: Int64(fields[5])!,
                                 created: Int64(fields[7])!, status: winner?.status)
        }
    }

    private static func parseStatus(_ value: String) -> (status: AgentStatus, since: Int64?)? {
        let parts = value.components(separatedBy: " ")
        guard parts.count <= 2 else { return nil }
        var since: Int64?
        if parts.count == 2 {
            guard parts[1].count <= 12, digits(parts[1]), let epoch = Int64(parts[1]), epoch > 0 else { return nil }
            since = epoch
        }
        let state = parts[0] == "blocked" ? "waiting" : parts[0]
        guard let status = AgentStatus(rawValue: state) else { return nil }
        return (status, since)
    }

    private static func creationArgument(_ value: String) -> String {
        // tmux also parses a trailing semicolon as a command separator.
        quote(value.hasSuffix(";") ? String(value.dropLast()) + "\\;" : value)
    }

    static func creation(name: String, directory: String, command: String) throws -> String {
        guard !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              name.unicodeScalars.count <= 64,
              !name.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) || $0 == "." || $0 == ":" }) else {
            throw SSHTransportError.message("Session names must be at most 64 characters, without controls, dots, or colons.")
        }
        for (label, value) in [("Directory", directory), ("Command", command)] {
            guard value.unicodeScalars.count <= 4096,
                  !value.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains) else {
                throw SSHTransportError.message("\(label) must be at most 4096 characters without controls.")
            }
        }
        guard directory.isEmpty || directory == "~" || directory.hasPrefix("~/") || directory.hasPrefix("/") else {
            throw SSHTransportError.message("Directory must be an absolute path, ~, or ~/path.")
        }
        // Probe encoding tools in a subshell: LC_ALL=C must not reach a newly
        // started tmux server or its future shells.
        var script = prelude + "(\n" + encodingPrelude + ") || exit $?\ndirectory=" + quote(directory) + "\n"
        script += #"""
        case "$directory" in
            ''|'~') directory=$HOME ;;
            '~/'*) directory=$HOME/${directory#??} ;;
        esac
        case "$directory" in
            /*) ;;
            *) printf '%s\n' 'The target computer HOME is not an absolute directory.' >&2; exit 1 ;;
        esac
        # cd validates before mutation and avoids tmux -c format/job expansion.
        if ! CDPATH= cd "$directory" 2>/dev/null; then
            printf '%s\n' 'The requested directory does not exist or is not accessible.' >&2
            exit 1
        fi
        created=$(command tmux -u -L default new-session -d -P -F '#{session_id} #{pid} #{start_time} #{session_created}' -s
        """#
        script += " " + creationArgument(name.replacingOccurrences(of: "#", with: "##")) + " --"
        if !command.isEmpty { script += " " + creationArgument(command) }
        script += ") || exit $?\nprintf '%s\\n' 'DELUX_CREATED_1' \"$created\"\n"
        return shell(script + inventory)
    }

    static func parseCreated(_ data: Data) throws -> RemoteSession {
        guard let text = String(data: data, encoding: .utf8) else { throw malformed() }
        let sections = text.split(separator: "\n", maxSplits: 2, omittingEmptySubsequences: false)
        guard sections.count == 3, sections[0] == "DELUX_CREATED_1" else { throw malformed() }
        let identity = sections[1].components(separatedBy: " ")
        guard identity.count == 4, validID(identity[0]) else { throw malformed() }
        let numbers = try identity.dropFirst().map { value -> Int64 in
            guard digits(value), let number = Int64(value), number > 0 else { throw malformed() }
            return number
        }
        let sessions = try parseInventory(Data(sections[2].utf8))
        guard let session = sessions.first(where: {
            $0.id == identity[0] && $0.serverPID == numbers[0] &&
            $0.serverStarted == numbers[1] && $0.created == numbers[2]
        }) else {
            throw SSHTransportError.message("The created session already ended or changed; its command may have exited.")
        }
        return session
    }

    static func attach(_ session: RemoteSession) throws -> String {
        guard validID(session.id), session.serverPID > 0, session.serverStarted > 0, session.created > 0 else {
            throw malformed()
        }
        // IDs are monotonic within a server. Do not format session_created on
        // a vanished target: older tmux servers can crash doing so. PID/start
        // plus ID identifies the lifetime, and if-shell -F tests and attaches
        // in one synchronous command queue without a lookup/attach race.
        let id = "#{==:#{session_id},\(session.id)}"
        let pid = "#{==:#{pid},\(session.serverPID)}"
        let start = "#{==:#{start_time},\(session.serverStarted)}"
        let condition = "#{&&:#{&&:\(id),\(pid)},\(start)}"
        let action = "attach-session -t " + quote(session.id)
        let stale = "display-message -p 'DELUX_STALE: selected session changed' ; run-shell 'exit 74'"
        let arguments = ["if-shell", "-F", "-t", session.id + ":", condition, action, stale]
        return shell(prelude + "exec tmux -u -N -L default " + arguments.map(quote).joined(separator: " "))
    }

    private static func malformed() -> SSHTransportError {
        .message("Malformed or incomplete tmux response. Refresh and try again.")
    }
}
