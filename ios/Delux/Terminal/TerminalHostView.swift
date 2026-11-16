import SwiftTerm
import SwiftUI
import UIKit

/// SwiftTerm `TerminalView` bridged into SwiftUI. Bytes from the controller are
/// fed verbatim; keystrokes and grid changes go back through the delegate.
struct TerminalHostView: UIViewRepresentable {
    let controller: TerminalSessionController
    let isConnected: Bool
    @Binding var keyboardShown: Bool

    func makeCoordinator() -> Coordinator {
        Coordinator(controller: controller, keyboardShown: $keyboardShown)
    }

    func makeUIView(context: Context) -> DeluxTerminalView {
        let view = DeluxTerminalView(frame: .zero, font: UIFont.monospacedSystemFont(ofSize: 13, weight: .regular))
        view.terminalDelegate = context.coordinator
        view.accessibilityIdentifier = AccessibilityID.terminal
        view.nativeBackgroundColor = UIColor.systemBackground
        view.nativeForegroundColor = UIColor.label
        view.caretColor = UIColor.tintColor
        view.inputAccessoryView = TerminalAccessoryBar(terminalView: view)
        view.onGridChange = { [weak coordinator = context.coordinator] columns, rows in
            coordinator?.gridChanged(columns: columns, rows: rows)
        }
        controller.attachOutput { [weak view] data in
            view?.feed(byteArray: ArraySlice(data))
        }
        context.coordinator.observeKeyboard(for: view)
        return view
    }

    func updateUIView(_ view: DeluxTerminalView, context: Context) {
        let coordinator = context.coordinator
        if isConnected, !coordinator.didFocusAfterConnect {
            coordinator.didFocusAfterConnect = true
            DispatchQueue.main.async {
                if !view.isFirstResponder { _ = view.becomeFirstResponder() }
            }
        } else if !isConnected {
            coordinator.didFocusAfterConnect = false
        }
        // Only react to the toolbar toggle; system-driven changes arrive through the keyboard notifications.
        guard keyboardShown != coordinator.appliedKeyboardShown else { return }
        coordinator.appliedKeyboardShown = keyboardShown
        let show = keyboardShown
        DispatchQueue.main.async {
            if show, !view.isFirstResponder {
                _ = view.becomeFirstResponder()
            } else if !show, view.isFirstResponder {
                _ = view.resignFirstResponder()
            }
        }
    }

    static func dismantleUIView(_ view: DeluxTerminalView, coordinator: Coordinator) {
        coordinator.stopObserving()
        coordinator.detachOutput()
        view.onGridChange = nil
        view.terminalDelegate = nil
    }

    @MainActor
    final class Coordinator: NSObject, TerminalViewDelegate {
        private let controller: TerminalSessionController
        private let keyboardShown: Binding<Bool>
        private var observers: [NSObjectProtocol] = []
        private var lastColumns = 0
        private var lastRows = 0
        var didFocusAfterConnect = false
        var appliedKeyboardShown = false

        init(controller: TerminalSessionController, keyboardShown: Binding<Bool>) {
            self.controller = controller
            self.keyboardShown = keyboardShown
        }

        func gridChanged(columns: Int, rows: Int) {
            guard columns != lastColumns || rows != lastRows else { return }
            lastColumns = columns
            lastRows = rows
            controller.viewResized(columns: columns, rows: rows)
        }

        func detachOutput() {
            controller.detachOutput()
        }

        /// Keeps the toggle honest when the system shows or hides the keyboard itself (swipe, hardware keyboard).
        func observeKeyboard(for view: UIView) {
            let center = NotificationCenter.default
            observers = [
                center.addObserver(forName: UIResponder.keyboardWillShowNotification, object: nil, queue: .main) { [weak self, weak view] _ in
                    MainActor.assumeIsolated {
                        guard let self, view?.isFirstResponder == true else { return }
                        self.appliedKeyboardShown = true
                        self.keyboardShown.wrappedValue = true
                    }
                },
                center.addObserver(forName: UIResponder.keyboardWillHideNotification, object: nil, queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated {
                        guard let self else { return }
                        self.appliedKeyboardShown = false
                        self.keyboardShown.wrappedValue = false
                    }
                },
            ]
        }

        func stopObserving() {
            observers.forEach(NotificationCenter.default.removeObserver)
            observers.removeAll()
        }

        // MARK: TerminalViewDelegate
        // SwiftTerm calls these on the main thread (its `send` asserts it); the protocol itself is not isolated.

        nonisolated func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
            MainActor.assumeIsolated { gridChanged(columns: newCols, rows: newRows) }
        }

        nonisolated func send(source: TerminalView, data: ArraySlice<UInt8>) {
            let bytes = Data(data)
            MainActor.assumeIsolated { controller.send(bytes) }
        }

        nonisolated func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {
            guard let url = URL(string: link), let scheme = url.scheme?.lowercased(),
                  scheme == "https" || scheme == "http" else { return }
            MainActor.assumeIsolated { UIApplication.shared.open(url) }
        }

        nonisolated func setTerminalTitle(source: TerminalView, title: String) {}
        nonisolated func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
        nonisolated func scrolled(source: TerminalView, position: Double) {}
        nonisolated func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
    }
}

/// Reports the emulator grid after every layout so the first attach uses the
/// real size even when it happens to equal SwiftTerm's 80x25 default.
final class DeluxTerminalView: TerminalView {
    var onGridChange: ((Int, Int) -> Void)?

    override func layoutSubviews() {
        super.layoutSubviews()
        guard bounds.width > 0, bounds.height > 0 else { return }
        let terminal = getTerminal()
        onGridChange?(terminal.cols, terminal.rows)
    }
}

/// Keys a phone keyboard lacks. Ctrl latches until the next key, matching SwiftTerm's `controlModifier`.
final class TerminalAccessoryBar: UIInputView, UIInputViewAudioFeedback {
    private weak var terminalView: TerminalView?
    private let controlButton = UIButton(type: .system)
    private var controlResetObserver: NSObjectProtocol?

    /// SwiftTerm posts this name when it consumes the latched Ctrl; the constant is not public.
    private static let controlModifierReset = Notification.Name("SwiftTerm.TerminalView.controlModifierReset")

    init(terminalView: TerminalView) {
        self.terminalView = terminalView
        super.init(frame: CGRect(x: 0, y: 0, width: 0, height: 44), inputViewStyle: .keyboard)
        allowsSelfSizing = true
        translatesAutoresizingMaskIntoConstraints = false
        buildKeys()
        controlResetObserver = NotificationCenter.default.addObserver(
            forName: TerminalAccessoryBar.controlModifierReset, object: terminalView, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.controlButton.isSelected = false }
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    deinit {
        if let controlResetObserver {
            NotificationCenter.default.removeObserver(controlResetObserver)
        }
    }

    var enableInputClicksWhenVisible: Bool { true }

    override var intrinsicContentSize: CGSize {
        CGSize(width: UIView.noIntrinsicMetric, height: 44)
    }

    private func buildKeys() {
        let scroll = UIScrollView()
        scroll.showsHorizontalScrollIndicator = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        let stack = UIStackView()
        stack.axis = .horizontal
        stack.spacing = Spacing.sm
        stack.alignment = .fill
        stack.translatesAutoresizingMaskIntoConstraints = false

        stack.addArrangedSubview(key("esc", id: "escape", label: "Escape") { [weak self] in self?.sendBytes([0x1b]) })
        stack.addArrangedSubview(key("tab", id: "tab", label: "Tab") { [weak self] in self?.sendBytes([0x09]) })
        configure(controlButton, title: "ctrl", id: "control", label: "Control")
        controlButton.addAction(UIAction { [weak self] _ in self?.toggleControl() }, for: .touchUpInside)
        stack.addArrangedSubview(controlButton)
        stack.addArrangedSubview(key("", symbol: "arrow.left", id: "left", label: "Left arrow") { [weak self] in self?.sendArrow(.left) })
        stack.addArrangedSubview(key("", symbol: "arrow.up", id: "up", label: "Up arrow") { [weak self] in self?.sendArrow(.up) })
        stack.addArrangedSubview(key("", symbol: "arrow.down", id: "down", label: "Down arrow") { [weak self] in self?.sendArrow(.down) })
        stack.addArrangedSubview(key("", symbol: "arrow.right", id: "right", label: "Right arrow") { [weak self] in self?.sendArrow(.right) })
        stack.addArrangedSubview(key("-", id: "dash", label: "Dash") { [weak self] in self?.sendBytes([0x2d]) })
        stack.addArrangedSubview(key("|", id: "pipe", label: "Pipe") { [weak self] in self?.sendBytes([0x7c]) })
        stack.addArrangedSubview(key("~", id: "tilde", label: "Tilde") { [weak self] in self?.sendBytes([0x7e]) })
        stack.addArrangedSubview(key("/", id: "slash", label: "Slash") { [weak self] in self?.sendBytes([0x2f]) })

        let hide = key("", symbol: "keyboard.chevron.compact.down", id: "hideKeyboard", label: "Hide keyboard") { [weak self] in
            _ = self?.terminalView?.resignFirstResponder()
        }

        scroll.addSubview(stack)
        addSubview(scroll)
        addSubview(hide)
        hide.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor, constant: Spacing.sm),
            scroll.trailingAnchor.constraint(equalTo: hide.leadingAnchor, constant: -Spacing.sm),
            scroll.topAnchor.constraint(equalTo: topAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor),
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: Spacing.xs),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -Spacing.xs),
            stack.heightAnchor.constraint(equalTo: scroll.frameLayoutGuide.heightAnchor, constant: -2 * Spacing.xs),
            hide.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -Spacing.sm),
            hide.topAnchor.constraint(equalTo: topAnchor, constant: Spacing.xs),
            hide.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -Spacing.xs),
        ])
    }

    private func key(_ title: String, symbol: String? = nil, id: String, label: String, action: @escaping () -> Void) -> UIButton {
        let button = UIButton(type: .system)
        configure(button, title: title, symbol: symbol, id: id, label: label)
        button.addAction(UIAction { _ in action() }, for: .touchUpInside)
        return button
    }

    private func configure(_ button: UIButton, title: String, symbol: String? = nil, id: String, label: String) {
        var configuration = UIButton.Configuration.gray()
        configuration.cornerStyle = .medium
        configuration.contentInsets = NSDirectionalEdgeInsets(top: Spacing.xs, leading: Spacing.md, bottom: Spacing.xs, trailing: Spacing.md)
        if let symbol {
            configuration.image = UIImage(systemName: symbol)
            configuration.preferredSymbolConfigurationForImage = UIImage.SymbolConfiguration(textStyle: .body)
        } else {
            configuration.attributedTitle = AttributedString(title, attributes: AttributeContainer([
                .font: UIFont.monospacedSystemFont(ofSize: UIFont.preferredFont(forTextStyle: .callout).pointSize, weight: .medium),
            ]))
        }
        button.configuration = configuration
        button.configurationUpdateHandler = { button in
            var updated = button.configuration
            updated?.baseBackgroundColor = button.isSelected ? UIColor.tintColor : UIColor.tertiarySystemFill
            updated?.baseForegroundColor = button.isSelected ? UIColor.systemBackground : UIColor.label
            button.configuration = updated
        }
        button.accessibilityIdentifier = AccessibilityID.terminalKey(id)
        button.accessibilityLabel = label
    }

    private func sendBytes(_ bytes: [UInt8]) {
        UIDevice.current.playInputClick()
        guard let terminalView else { return }
        if terminalView.controlModifier, bytes.count == 1, let control = TerminalAccessoryBar.controlByte(bytes[0]) {
            terminalView.controlModifier = false
            terminalView.send([control])
        } else {
            terminalView.send(bytes)
        }
    }

    private enum Arrow { case up, down, left, right }

    private func sendArrow(_ arrow: Arrow) {
        guard let terminalView else { return }
        let application = terminalView.getTerminal().applicationCursor
        let sequence: [UInt8]
        switch arrow {
        case .up: sequence = application ? EscapeSequences.moveUpApp : EscapeSequences.moveUpNormal
        case .down: sequence = application ? EscapeSequences.moveDownApp : EscapeSequences.moveDownNormal
        case .left: sequence = application ? EscapeSequences.moveLeftApp : EscapeSequences.moveLeftNormal
        case .right: sequence = application ? EscapeSequences.moveRightApp : EscapeSequences.moveRightNormal
        }
        UIDevice.current.playInputClick()
        terminalView.send(sequence)
    }

    private func toggleControl() {
        guard let terminalView else { return }
        terminalView.controlModifier.toggle()
        controlButton.isSelected = terminalView.controlModifier
        UIDevice.current.playInputClick()
    }

    /// ASCII control code for a latched Ctrl on a printable key (Ctrl+- is 0x1f, Ctrl+/ is 0x1f too).
    private static func controlByte(_ byte: UInt8) -> UInt8? {
        switch byte {
        case 0x40...0x5f: return byte - 0x40
        case 0x61...0x7a: return byte - 0x60
        case 0x2d, 0x2f: return 0x1f
        case 0x7c: return 0x1c
        case 0x7e: return 0x1e
        default: return nil
        }
    }
}
