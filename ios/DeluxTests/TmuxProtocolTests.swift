import Foundation
import XCTest
@testable import Delux

final class TmuxProtocolTests: XCTestCase {
    private func fields(status: String = "", output: String = "1700000003") -> [String] {
        ["$7", "work", "2", "0", "1234", "1700000000", "/tmp/tmux-501/default",
         "1700000001", "1700000002", output, "sh", "/tmp/work", status]
    }

    private func frame(_ rows: [[String]]) -> Data {
        let records = rows.map { row in
            row.map { Data($0.utf8).base64EncodedString() }.joined(separator: "\t")
        }
        return Data((["DELUX_SESSIONS_4"] + records + ["DELUX_END", ""]).joined(separator: "\n").utf8)
    }

    private func created(_ identity: String, inventory: Data) -> Data {
        Data("DELUX_CREATED_1\n\(identity)\n".utf8) + inventory
    }

    func testEncodedNamesCannotInjectRecordsAndLifetimeIsPreserved() throws {
        var row = fields(status: "done\nworking\nerror\nblocked")
        row[1] = "雪\tname\nDELUX_END\n\n"
        let session = try XCTUnwrap(TmuxProtocol.parseInventory(frame([row]), now: 1700000004).first)
        XCTAssertEqual(session.name, row[1])
        XCTAssertEqual(session.status, .waiting)
        XCTAssertEqual(session.serverPID, 1234)
        XCTAssertEqual(session.serverStarted, 1700000000)
        XCTAssertEqual(session.created, 1700000001)
    }

    func testFramesRejectDuplicatesTruncationAndNoncanonicalEncoding() throws {
        XCTAssertThrowsError(try TmuxProtocol.parseInventory(frame([fields(), fields()])))
        let good = String(decoding: frame([fields()]), as: UTF8.self)
        for bad in [good.replacingOccurrences(of: "DELUX_END\n", with: ""),
                    "banner\n" + good, good + "trailer\n"] {
            XCTAssertThrowsError(try TmuxProtocol.parseInventory(Data(bad.utf8)))
        }
        var encoded = fields().map { Data($0.utf8).base64EncodedString() }
        encoded[1] = "AB==" // Decodes, but is not canonical base64.
        let noncanonical = "DELUX_SESSIONS_4\n" + encoded.joined(separator: "\t") + "\nDELUX_END\n"
        XCTAssertThrowsError(try TmuxProtocol.parseInventory(Data(noncanonical.utf8)))
        var invalid = fields()
        invalid[4] = "9223372036854775808"
        XCTAssertThrowsError(try TmuxProtocol.parseInventory(frame([invalid])))
        invalid = fields()
        invalid[1] = "nul\0name"
        XCTAssertThrowsError(try TmuxProtocol.parseInventory(frame([invalid])))
    }

    func testStatusGrammarPriorityAndStaleWorking() throws {
        let cases: [(String, AgentStatus?)] = [
            ("done\nworking\nerror\nwaiting", .waiting),
            ("idle\nblocked", .waiting), ("error\nworking", .error),
            ("idle\ndone", .done), ("idle", nil),
            ("Working\nwaiting  1\nworking 0\ndone\t9", nil),
            ("working 1700000001", .working)
        ]
        for (value, expected) in cases {
            XCTAssertEqual(try TmuxProtocol.parseInventory(frame([fields(status: value)]), now: 1700000004).first?.status,
                           expected, value)
        }
        XCTAssertNil(try TmuxProtocol.parseInventory(frame([fields(status: "working")]), now: 1700000604).first?.status)
        XCTAssertEqual(try TmuxProtocol.parseInventory(frame([fields(status: "working")]), now: 1700000603).first?.status, .working)
        XCTAssertEqual(try TmuxProtocol.parseInventory(frame([fields(status: "waiting 1")]), now: 1700009999).first?.status, .waiting)
        XCTAssertTrue(TmuxProtocol.inventory.contains("#{?pane_program_state,#{pane_program_state},"))
        XCTAssertTrue(TmuxProtocol.inventory.contains("#{m/r:[^a-z0-9 ],#{@agent_status}}"))
    }

    func testCreationRequiresExactReturnedLifetime() throws {
        let identity = "$7 1234 1700000000 1700000001"
        let session = try TmuxProtocol.parseCreated(created(identity, inventory: frame([fields()])))
        XCTAssertEqual(session.id, "$7")
        for index in [0, 4, 5, 7] {
            var replacement = fields()
            replacement[index] = index == 0 ? "$8" : String(Int64(replacement[index])! + 1)
            XCTAssertThrowsError(try TmuxProtocol.parseCreated(created(identity, inventory: frame([replacement]))))
        }
        for bad in ["$7 +1234 1700000000 1700000001", "$7 0 1700000000 1700000001", identity + " extra"] {
            XCTAssertThrowsError(try TmuxProtocol.parseCreated(created(bad, inventory: frame([fields()]))))
        }
        XCTAssertThrowsError(try TmuxProtocol.parseCreated(created(identity, inventory: frame([]))))
    }

    func testCreationRejectsInvalidNamesAndDirectories() throws {
        for directory in ["relative", "~someone", "../work", "/tmp/nul\0"] {
            XCTAssertThrowsError(try TmuxProtocol.creation(name: "work", directory: directory, command: ""))
        }
        for name in ["", "  ", "a.b", "a:b", "line\nbreak", String(repeating: "x", count: 65)] {
            XCTAssertThrowsError(try TmuxProtocol.creation(name: name, directory: "~", command: ""))
        }
    }

    func testAttachRejectsInjectedOrInvalidIdentity() throws {
        var session = try XCTUnwrap(TmuxProtocol.parseInventory(frame([fields()])).first)
        session.id = "$7;touch /tmp/no"
        XCTAssertThrowsError(try TmuxProtocol.attach(session))
        session.id = "$7"
        session.serverStarted = 0
        XCTAssertThrowsError(try TmuxProtocol.attach(session))
    }
}
