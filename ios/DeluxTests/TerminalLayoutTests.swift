import UIKit
import XCTest
@testable import Delux

@MainActor
final class TerminalLayoutTests: XCTestCase {
    func testPortraitKeyboardTransitionsKeepTerminalAboveAccessory() {
        assertKeyboardTransitions(size: CGSize(width: 402, height: 874), top: 116,
                                  heights: [352, 78, 352, 0])
    }

    func testLandscapeKeyboardTransitionsKeepTerminalAboveAccessory() {
        assertKeyboardTransitions(size: CGSize(width: 874, height: 402), top: 78,
                                  heights: [208, 64, 208, 0])
    }

    private func assertKeyboardTransitions(size: CGSize, top: CGFloat, heights: [CGFloat],
                                           file: StaticString = #filePath, line: UInt = #line) {
        guard let scene = UIApplication.shared.connectedScenes.compactMap({ $0 as? UIWindowScene }).first else {
            XCTFail("The app test host has no window scene", file: file, line: line)
            return
        }
        let window = UIWindow(windowScene: scene)
        window.frame = CGRect(origin: .zero, size: size)
        let root = UIViewController()
        window.rootViewController = root
        window.isHidden = false
        defer { window.isHidden = true }
        window.layoutIfNeeded()

        let terminal = DeluxTerminalView(frame: .zero,
                                        font: UIFont.monospacedSystemFont(ofSize: 13, weight: .regular))
        let container = TerminalContainerView(terminal: terminal)
        root.view.addSubview(container)
        container.frame = CGRect(x: 0, y: top, width: size.width, height: size.height - top)
        container.layoutIfNeeded()
        guard container.window === window else {
            XCTFail("The layout fixture is not attached to its window", file: file, line: line)
            return
        }

        for height in heights {
            let frame = CGRect(x: 0, y: size.height - height, width: size.width, height: height)
            let screenFrame = window.convert(frame, to: window.screen.coordinateSpace)
            container.keyboardChanged(Notification(
                name: UIResponder.keyboardDidChangeFrameNotification,
                object: nil,
                userInfo: [UIResponder.keyboardFrameEndUserInfoKey: NSValue(cgRect: screenFrame)]
            ))
            container.layoutIfNeeded()
            let terminalFrame = terminal.convert(terminal.bounds, to: window)
            let safeFrame = container.convert(container.safeAreaLayoutGuide.layoutFrame, to: window)
            XCTAssertEqual(terminalFrame.maxY, min(frame.minY, safeFrame.maxY), accuracy: 0.5,
                           "Keyboard height \(height)", file: file, line: line)
            XCTAssertEqual(terminalFrame.minY, top, accuracy: 0.5, file: file, line: line)
        }
    }
}
