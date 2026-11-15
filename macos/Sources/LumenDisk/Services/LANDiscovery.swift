import Foundation
import Network
import Observation

/// A LumenDisk share discovered on the local network (`_lumendisk._tcp`).
struct LANPeer: Identifiable, Sendable, Hashable {
    /// Bonjour service name (unique per domain).
    var id: String
    /// Volume name from the TXT record, else the service name.
    var name: String
    /// Filesystem from the TXT record ("EXT4", "NTFS"…), if any.
    var fsType: String?
    /// Volume id on the sharing Mac (TXT `id`).
    var remoteVolumeID: String?
    /// SHA-256 of the share's TLS certificate (TXT `fp`): the share speaks https and the core
    /// pins this certificate. Nil for older, plain-http shares.
    var fingerprint: String?

    var scheme: String { fingerprint == nil ? "http" : "https" }

    var filesystem: FilesystemKind { FilesystemKind(coreName: fsType) }
}

enum LANDiscoveryError: LocalizedError, Sendable {
    case notFound
    case timeout
    case failed(String)

    var errorDescription: String? {
        switch self {
        case .notFound: L("Questo Mac non è più visibile nella rete locale.", "This Mac is no longer visible on your network.")
        case .timeout: L("Il Mac che condivide il disco non ha risposto in tempo.", "The sharing Mac did not respond in time.")
        case .failed(let detail): L("Impossibile raggiungere il Mac che condivide il disco (\(detail)).", "Could not reach the sharing Mac (\(detail)).")
        }
    }
}

/// Browses Bonjour for other Macs sharing LumenDisk volumes, using `NWBrowser`.
@MainActor
@Observable
final class LANDiscovery {
    private(set) var peers: [LANPeer] = []
    private(set) var isBrowsing = false

    /// Volume ids this Mac is sharing: hidden from the list (no point connecting to ourselves).
    @ObservationIgnored var localSharedVolumeIDs: Set<String> = [] {
        didSet { if localSharedVolumeIDs != oldValue { publishPeers() } }
    }

    @ObservationIgnored private var browser: NWBrowser?
    @ObservationIgnored private var endpoints: [String: NWEndpoint] = [:]
    @ObservationIgnored private var discovered: [LANPeer] = []

    func start() {
        guard browser == nil else { return }
        let browser = NWBrowser(for: .bonjourWithTXTRecord(type: "_lumendisk._tcp", domain: "local."), using: .tcp)
        browser.stateUpdateHandler = { [weak self] state in
            MainActor.assumeIsolated {
                switch state {
                case .ready: self?.isBrowsing = true
                case .failed, .cancelled: self?.isBrowsing = false
                default: break
                }
            }
        }
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            MainActor.assumeIsolated { self?.update(results) }
        }
        browser.start(queue: .main)
        self.browser = browser
    }

    func stop() {
        browser?.cancel()
        browser = nil
        isBrowsing = false
    }

    private func update(_ results: Set<NWBrowser.Result>) {
        var found: [LANPeer] = []
        var newEndpoints: [String: NWEndpoint] = [:]
        for result in results {
            guard case .service(let serviceName, _, _, _) = result.endpoint else { continue }
            var txt: [String: String] = [:]
            if case .bonjour(let record) = result.metadata { txt = record.dictionary }
            let name = txt["name"].flatMap { $0.isEmpty ? nil : $0 } ?? serviceName
            let fingerprint = txt["fp"].flatMap { $0.isEmpty ? nil : $0.lowercased() }
            found.append(LANPeer(id: serviceName, name: name, fsType: txt["fs"], remoteVolumeID: txt["id"], fingerprint: fingerprint))
            newEndpoints[serviceName] = result.endpoint
        }
        endpoints = newEndpoints
        discovered = found.sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
        publishPeers()
    }

    private func publishPeers() {
        let visible = discovered.filter { peer in
            guard let id = peer.remoteVolumeID else { return true }
            return !localSharedVolumeIDs.contains(id)
        }
        if visible != peers { peers = visible }
    }

    func peer(id: String) -> LANPeer? { peers.first { $0.id == id } }

    /// Resolves a discovered service to `<scheme>://<host>:<port>` (IPv4 preferred) by opening
    /// a short-lived TCP connection and reading the remote endpoint. Shares that publish a TLS
    /// fingerprint are reached over https.
    func baseURL(for peer: LANPeer) async throws -> String {
        guard let endpoint = endpoints[peer.id] else { throw LANDiscoveryError.notFound }
        let hostPort: String
        do {
            hostPort = try await Self.resolve(endpoint, ipv4Only: true)
        } catch {
            hostPort = try await Self.resolve(endpoint, ipv4Only: false)
        }
        return peer.scheme + "://" + hostPort
    }

    /// Resumes its continuation exactly once (ready, failure or timeout).
    private final class EndpointResolver: @unchecked Sendable {
        let connection: NWConnection
        private let lock = NSLock()
        private var continuation: CheckedContinuation<String, any Error>?

        init(connection: NWConnection, continuation: CheckedContinuation<String, any Error>) {
            self.connection = connection
            self.continuation = continuation
        }

        func finish(_ result: Result<String, any Error>) {
            lock.lock()
            let pending = continuation
            continuation = nil
            lock.unlock()
            guard let pending else { return }
            connection.stateUpdateHandler = nil
            connection.cancel()
            pending.resume(with: result)
        }
    }

    private nonisolated static func resolve(_ endpoint: NWEndpoint, ipv4Only: Bool) async throws -> String {
        let parameters = NWParameters.tcp
        if ipv4Only, let ip = parameters.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options {
            ip.version = .v4
        }
        let connection = NWConnection(to: endpoint, using: parameters)
        return try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<String, any Error>) in
            let resolver = EndpointResolver(connection: connection, continuation: continuation)
            connection.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    if case .hostPort(let host, let port)? = resolver.connection.currentPath?.remoteEndpoint {
                        resolver.finish(.success("\(format(host)):\(port.rawValue)"))
                    } else {
                        resolver.finish(.failure(LANDiscoveryError.failed("endpoint")))
                    }
                case .failed(let error), .waiting(let error):
                    resolver.finish(.failure(LANDiscoveryError.failed(error.localizedDescription)))
                default:
                    break
                }
            }
            connection.start(queue: .global(qos: .userInitiated))
            DispatchQueue.global().asyncAfter(deadline: .now() + 6) {
                resolver.finish(.failure(LANDiscoveryError.timeout))
            }
        }
    }

    private nonisolated static func format(_ host: NWEndpoint.Host) -> String {
        switch host {
        case .ipv4(let address):
            return "\(address)".components(separatedBy: "%").first ?? "\(address)"
        case .ipv6(let address):
            let text = "\(address)"
            // Link-local scope ids must be percent-encoded inside a URL ("%25en0").
            return "[" + text.replacingOccurrences(of: "%", with: "%25") + "]"
        case .name(let name, _):
            return name
        @unknown default:
            return "\(host)"
        }
    }
}

/// Publishes one `_lumendisk._tcp` Bonjour service per shared volume (NetService), as required
/// by the core: the core serves WebDAV on `ShareStatus.port`, the app advertises it.
@MainActor
final class BonjourPublisher: NSObject, NetServiceDelegate {
    private struct Published {
        var service: NetService
        var port: UInt16
        var fingerprint: String?
    }

    private var services: [String: Published] = [:]
    var isEnabled = true

    var publishedVolumeIDs: Set<String> { Set(services.keys) }

    /// Makes the published services match `shares` (volume id → (volume, share)).
    func reconcile(_ shares: [String: (VolumeStatus, ShareStatus)]) {
        for (id, published) in services
        where shares[id] == nil || shares[id]?.1.port != published.port || shares[id]?.1.fingerprint != published.fingerprint {
            published.service.stop()
            services[id] = nil
        }
        guard isEnabled else { return }
        for (id, entry) in shares where services[id] == nil && entry.1.port > 0 {
            let (volume, share) = entry
            let name = share.serviceName.isEmpty ? volume.displayName : share.serviceName
            let service = NetService(domain: "local.", type: "_lumendisk._tcp.", name: name, port: Int32(share.port))
            var txt: [String: Data] = [
                "v": Data("1".utf8),
                "name": Data(volume.displayName.utf8),
                "fs": Data(volume.fsType.utf8),
                "id": Data(volume.id.utf8),
            ]
            // TLS: clients pin the certificate and switch to https.
            if let fingerprint = share.fingerprint { txt["fp"] = Data(fingerprint.utf8) }
            service.setTXTRecord(NetService.data(fromTXTRecord: txt))
            service.delegate = self
            service.publish()
            services[id] = Published(service: service, port: share.port, fingerprint: share.fingerprint)
        }
    }

    func stopAll() {
        for published in services.values { published.service.stop() }
        services.removeAll()
    }

    nonisolated func netService(_ sender: NetService, didNotPublish errorDict: [String: NSNumber]) {
        NSLog("LumenDisk: Bonjour publish failed: \(errorDict)")
    }
}
