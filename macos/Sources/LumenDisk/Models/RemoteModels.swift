import Foundation

// MARK: - Phase 2: network & remote storage payloads
//
// Every type decodes leniently: fields may be missing on older cores, and unknown enum
// spellings fall back to a sensible default instead of failing the whole volume list.

/// Where a volume's data lives (`VolumeStatus.kind`).
enum VolumeKind: String, Codable, Sendable, Hashable, CaseIterable {
    case local
    case imageURL = "image_url"
    case sftp
    case webdav
    case lan

    init(from decoder: any Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        switch raw.lowercased().replacingOccurrences(of: "-", with: "_") {
        case "image_url", "imageurl", "url": self = .imageURL
        case "sftp", "ssh": self = .sftp
        case "webdav", "dav": self = .webdav
        case "lan", "share": self = .lan
        default: self = .local
        }
    }

    /// Used when an older core does not report `kind`.
    static func infer(sourcePath: String, hasRemote: Bool) -> VolumeKind {
        let lower = sourcePath.lowercased()
        if lower.hasPrefix("sftp://") { return .sftp }
        if lower.hasPrefix("http://") || lower.hasPrefix("https://") {
            let path = URL(string: sourcePath)?.path().lowercased() ?? lower
            return ImagePicker.extensions.contains(where: { path.hasSuffix("." + $0) }) ? .imageURL : .webdav
        }
        return hasRemote ? .webdav : .local
    }

    var isRemote: Bool { self != .local }

    /// Short pill text for remote kinds.
    var badge: String {
        switch self {
        case .local: ""
        case .imageURL: "URL"
        case .sftp: "SFTP"
        case .webdav: "WebDAV"
        case .lan: "LAN"
        }
    }

    var title: String {
        switch self {
        case .local: L("Disco locale", "Local disk")
        case .imageURL: L("Immagine su Internet", "Image on the web")
        case .sftp: L("Server SFTP", "SFTP server")
        case .webdav: L("Server WebDAV", "WebDAV server")
        case .lan: L("Mac nella rete locale", "Mac on your network")
        }
    }

    var symbol: String {
        switch self {
        case .local: "externaldrive.fill"
        case .imageURL: "arrow.down.circle.fill"
        case .sftp: "server.rack"
        case .webdav: "globe"
        case .lan: "desktopcomputer"
        }
    }
}

struct RemoteStatus: Codable, Sendable, Hashable {
    var url: String = ""
    var online: Bool = true
    var latencyMs: UInt32?
    var pendingOps: UInt64 = 0
    var pendingFiles: [String] = []
    var lastSync: Int64?
    var lastError: String?
    var cacheBytes: UInt64 = 0
    var fetchedBytes: UInt64 = 0
    var remoteSize: UInt64?

    init() {}

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        url = (try? c.decodeIfPresent(String.self, forKey: .url)) ?? ""
        online = (try? c.decodeIfPresent(Bool.self, forKey: .online)) ?? true
        latencyMs = (try? c.decodeIfPresent(UInt32.self, forKey: .latencyMs))
        pendingOps = (try? c.decodeIfPresent(UInt64.self, forKey: .pendingOps)) ?? 0
        pendingFiles = (try? c.decodeIfPresent([String].self, forKey: .pendingFiles)) ?? []
        lastSync = (try? c.decodeIfPresent(Int64.self, forKey: .lastSync))
        lastError = (try? c.decodeIfPresent(String.self, forKey: .lastError))
        cacheBytes = (try? c.decodeIfPresent(UInt64.self, forKey: .cacheBytes)) ?? 0
        fetchedBytes = (try? c.decodeIfPresent(UInt64.self, forKey: .fetchedBytes)) ?? 0
        remoteSize = (try? c.decodeIfPresent(UInt64.self, forKey: .remoteSize))
    }
}

struct ShareStatus: Codable, Sendable, Hashable {
    var port: UInt16 = 0
    var addresses: [String] = []
    var pairingCode: String = ""
    var serviceName: String = ""
    var clients: UInt32 = 0
    var bytesSent: UInt64 = 0
    var bytesReceived: UInt64 = 0
    // Phase 3 (TLS): absent on older cores.
    var scheme: String = "http"
    /// SHA-256 of the share's TLS certificate (hex, lowercase); published as TXT `fp=`.
    var fingerprint: String?

    init() {}

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        port = (try? c.decodeIfPresent(UInt16.self, forKey: .port)) ?? 0
        addresses = (try? c.decodeIfPresent([String].self, forKey: .addresses)) ?? []
        pairingCode = (try? c.decodeIfPresent(String.self, forKey: .pairingCode)) ?? ""
        serviceName = (try? c.decodeIfPresent(String.self, forKey: .serviceName)) ?? ""
        clients = (try? c.decodeIfPresent(UInt32.self, forKey: .clients)) ?? 0
        bytesSent = (try? c.decodeIfPresent(UInt64.self, forKey: .bytesSent)) ?? 0
        bytesReceived = (try? c.decodeIfPresent(UInt64.self, forKey: .bytesReceived)) ?? 0
        fingerprint = (try? c.decodeIfPresent(String.self, forKey: .fingerprint))?.nonEmpty
        let declared = (try? c.decodeIfPresent(String.self, forKey: .scheme))?.lowercased()
        scheme = declared ?? (fingerprint != nil ? "https" : "http")
    }

    /// True when the share is served over TLS (encrypted).
    var isEncrypted: Bool { scheme == "https" }

    /// IPv4 addresses first: they are the ones people can read and type.
    var preferredAddress: String? {
        addresses.first { !$0.contains(":") } ?? addresses.first
    }
}

struct PortalStatus: Codable, Sendable, Hashable {
    var url: String = ""
    var pin: String = ""
    var port: UInt16 = 0
    var uploads: UInt64 = 0
    var downloads: UInt64 = 0
    // Phase 3 (TLS): `url` is https, `httpUrl` the unencrypted compatibility address.
    var httpUrl: String?
    var fingerprint: String?

    init() {}

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        url = (try? c.decodeIfPresent(String.self, forKey: .url)) ?? ""
        pin = (try? c.decodeIfPresent(String.self, forKey: .pin)) ?? ""
        port = (try? c.decodeIfPresent(UInt16.self, forKey: .port)) ?? 0
        uploads = (try? c.decodeIfPresent(UInt64.self, forKey: .uploads)) ?? 0
        downloads = (try? c.decodeIfPresent(UInt64.self, forKey: .downloads)) ?? 0
        httpUrl = (try? c.decodeIfPresent(String.self, forKey: .httpUrl))?.nonEmpty
        fingerprint = (try? c.decodeIfPresent(String.self, forKey: .fingerprint))?.nonEmpty
    }

    var isEncrypted: Bool { url.lowercased().hasPrefix("https://") }
    /// The unencrypted address, only when it differs from the main one.
    var compatibilityURL: String? {
        guard let httpUrl, httpUrl != url else { return nil }
        return httpUrl
    }
}

struct JournalStatus: Codable, Sendable, Hashable {
    var enabled: Bool = false
    var transactions: UInt64 = 0
    var replayedAtMount: UInt64 = 0
    // Phase 3
    var mode: String?
    var walBytes: UInt64?
    var dataBytes: UInt64?

    init() {}

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        enabled = (try? c.decodeIfPresent(Bool.self, forKey: .enabled)) ?? false
        transactions = (try? c.decodeIfPresent(UInt64.self, forKey: .transactions)) ?? 0
        replayedAtMount = (try? c.decodeIfPresent(UInt64.self, forKey: .replayedAtMount)) ?? 0
        mode = (try? c.decodeIfPresent(String.self, forKey: .mode))?.nonEmpty
        walBytes = try? c.decodeIfPresent(UInt64.self, forKey: .walBytes)
        dataBytes = try? c.decodeIfPresent(UInt64.self, forKey: .dataBytes)
    }
}

/// Reply of ld_inspect_image_url.
struct ImageURLInspection: Codable, Sendable, Hashable {
    var path: String
    var size: UInt64
    var partitions: [PartitionInfo]
    var fetchedBytes: UInt64

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        path = (try? c.decodeIfPresent(String.self, forKey: .path)) ?? ""
        size = (try? c.decodeIfPresent(UInt64.self, forKey: .size)) ?? 0
        partitions = try c.decodeIfPresent([PartitionInfo].self, forKey: .partitions) ?? []
        fetchedBytes = (try? c.decodeIfPresent(UInt64.self, forKey: .fetchedBytes)) ?? 0
    }
}

/// Reply of ld_probe_remote.
struct ProbeResult: Codable, Sendable, Hashable {
    var latencyMs: UInt32?
    var entries: UInt32?
}

/// Reply of ld_remote_sync_now.
struct SyncResult: Codable, Sendable, Hashable {
    var synced: UInt64?
    var pending: UInt64?
}

// MARK: - Requests

enum RemoteProtocol: String, Codable, Sendable, CaseIterable, Identifiable {
    case sftp, webdav, lan
    var id: String { rawValue }
    var title: String {
        switch self {
        case .sftp: "SFTP"
        case .webdav: "WebDAV"
        case .lan: "LAN"
        }
    }
}

/// RemoteMountRequest (ld_mount_remote, ld_probe_remote). Optional fields are sent as explicit
/// nulls, as the header documents them as `str|null`.
struct RemoteMountRequest: Encodable, Sendable, Hashable {
    var kind: RemoteProtocol
    var url: String
    var password: String? = nil
    var identityFile: String? = nil
    var username: String? = nil
    var pairingCode: String? = nil
    /// lan: SHA-256 of the share's certificate (Bonjour TXT `fp=`), pinned by the core.
    var fingerprint: String? = nil
    var mode: MountMode = .readWrite
    var ghostShield: Bool = true
    var name: String? = nil
    var mountRoot: String? = AppPaths.defaultMountRoot
    var volumeIcon: String? = AppPaths.volumeIconPath

    func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(kind.rawValue, forKey: .kind)
        try c.encode(url, forKey: .url)
        try c.encode(password, forKey: .password)
        try c.encode(identityFile, forKey: .identityFile)
        try c.encode(username, forKey: .username)
        try c.encode(pairingCode, forKey: .pairingCode)
        try c.encode(fingerprint, forKey: .fingerprint)
        try c.encode(mode.rawValue, forKey: .mode)
        try c.encode(ghostShield, forKey: .ghostShield)
        try c.encode(name, forKey: .name)
        try c.encode(mountRoot, forKey: .mountRoot)
        try c.encode(volumeIcon, forKey: .volumeIcon)
    }
    enum CodingKeys: String, CodingKey {
        case kind, url, password, identityFile, username, pairingCode, fingerprint, mode, ghostShield, name, mountRoot, volumeIcon
    }
}

/// ImageUrlRequest (ld_inspect_image_url, ld_mount_image_url).
struct ImageURLRequest: Encodable, Sendable, Hashable {
    var url: String
    var password: String? = nil
    var identityFile: String? = nil
    var partition: UInt32 = 0
    /// Only `.shadow` or `.readOnly`: the remote file is never modified.
    var mode: MountMode = .shadow
    var ghostShield: Bool = true
    var name: String? = nil
    var mountRoot: String? = AppPaths.defaultMountRoot
    var volumeIcon: String? = AppPaths.volumeIconPath

    func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(url, forKey: .url)
        try c.encode(password, forKey: .password)
        try c.encode(identityFile, forKey: .identityFile)
        try c.encode(partition, forKey: .partition)
        try c.encode(mode == .readOnly ? MountMode.readOnly.rawValue : MountMode.shadow.rawValue, forKey: .mode)
        try c.encode(ghostShield, forKey: .ghostShield)
        try c.encode(name, forKey: .name)
        try c.encode(mountRoot, forKey: .mountRoot)
        try c.encode(volumeIcon, forKey: .volumeIcon)
    }
    enum CodingKeys: String, CodingKey {
        case url, password, identityFile, partition, mode, ghostShield, name, mountRoot, volumeIcon
    }
}
