import Foundation
import Observation

struct NearbyComputer: Identifiable, Equatable {
    let id: String
    let name: String
    let address: SSHAddress
}

/// Advertisements are suggestions, not proof of a computer's identity.
/// Pairing a route never changes the saved SSH host-key fingerprint.
@MainActor
@Observable
// Foundation invokes these delegates on the run loop where browsing/resolution
// starts. Both are started on the main actor; enforce that at callback entry.
final class NearbyComputers: NSObject, @preconcurrency NetServiceBrowserDelegate, @preconcurrency NetServiceDelegate {
    private(set) var computers: [NearbyComputer] = []
    private(set) var error: String?
    @ObservationIgnored private var browser: NetServiceBrowser?
    @ObservationIgnored private var services: [String: NetService] = [:]

    func start() {
        guard browser == nil else { return }
        error = nil
        let browser = NetServiceBrowser()
        self.browser = browser
        browser.delegate = self
        browser.searchForServices(ofType: "_ssh._tcp.", inDomain: "local.")
    }

    func stop() {
        browser?.delegate = nil
        browser?.stop()
        browser = nil
        for service in services.values {
            service.delegate = nil
            service.stop()
        }
        services.removeAll()
        computers.removeAll()
    }

    private func identity(_ service: NetService) -> String {
        "\(service.name).\(service.type)\(service.domain)"
    }

    func netServiceBrowser(_ browser: NetServiceBrowser, didFind service: NetService, moreComing: Bool) {
        guard self.browser === browser else { return }
        let id = identity(service)
        guard services[id] == nil else { return }
        services[id] = service
        service.delegate = self
        service.resolve(withTimeout: 3)
    }

    func netServiceBrowser(_ browser: NetServiceBrowser, didRemove service: NetService, moreComing: Bool) {
        guard self.browser === browser else { return }
        let id = identity(service)
        let old = services.removeValue(forKey: id)
        old?.delegate = nil
        old?.stop()
        computers.removeAll { $0.id == id }
    }

    func netServiceBrowser(_ browser: NetServiceBrowser, didNotSearch errorDict: [String: NSNumber]) {
        guard self.browser === browser else { return }
        stop()
        error = "Nearby discovery is unavailable. Allow Local Network access in Settings, or enter an address manually."
    }

    func netServiceDidResolveAddress(_ sender: NetService) {
        let id = identity(sender)
        guard services[id] === sender, let host = sender.hostName,
              !host.isEmpty, (1...65535).contains(sender.port) else { return }
        let hostname = host.hasSuffix(".") ? String(host.dropLast()) : host
        let computer = NearbyComputer(id: id, name: sender.name,
                                      address: SSHAddress(hostname: hostname, port: sender.port))
        computers.removeAll { $0.id == id }
        computers.append(computer)
        computers.sort {
            let order = $0.name.localizedStandardCompare($1.name)
            return order == .orderedSame ? $0.id < $1.id : order == .orderedAscending
        }
    }

    func netService(_ sender: NetService, didNotResolve errorDict: [String: NSNumber]) {
        guard services[identity(sender)] === sender else { return }
        computers.removeAll { $0.id == identity(sender) }
    }
}
