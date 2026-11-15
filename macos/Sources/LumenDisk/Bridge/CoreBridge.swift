import CLumenDisk
import Foundation

/// Errors surfaced by the bridge. `message` is already user-presentable (the core's text or a
/// translated fallback).
enum CoreError: LocalizedError, Sendable, Equatable {
    case notInitialized
    case nullResponse
    case malformedResponse(String)
    case core(String)

    var errorDescription: String? {
        switch self {
        case .notInitialized: L("Il motore di LumenDisk non è ancora pronto.", "The LumenDisk engine is not ready yet.")
        case .nullResponse: L("Il motore di LumenDisk non ha risposto.", "The LumenDisk engine did not respond.")
        case .malformedResponse(let detail): L("Risposta inattesa dal motore di LumenDisk (\(detail)).", "Unexpected reply from the LumenDisk engine (\(detail)).")
        case .core(let message): message
        }
    }

    /// Heuristic used to offer the lock inspector after a failed unmount.
    var looksLikeBusy: Bool {
        guard case .core(let message) = self else { return false }
        let m = message.lowercased()
        return ["busy", "in uso", "ebusy", "resource", "occupat", "open files"].contains { m.contains($0) }
    }
}

// MARK: - Reply payloads

private struct Envelope: Decodable { var ok: Bool; var error: String? }
private struct VersionReply: Decodable { var version: String? }
private struct VolumeReply: Decodable, Sendable { var volume: VolumeStatus }
private struct VolumesReply: Decodable, Sendable { var volumes: [VolumeStatus] }
private struct ChangesReply: Decodable, Sendable { var changes: [ShadowChange] }
private struct AppliedReply: Decodable, Sendable { var applied: UInt64? }
private struct DiscardedReply: Decodable, Sendable { var discarded: UInt64? }
private struct SessionsReply: Decodable, Sendable { var sessions: [ShadowSession]? }
private struct EntriesReply: Decodable, Sendable { var entries: [GhostEntry] }
private struct HoldersReply: Decodable, Sendable { var holders: [LockHolder] }
private struct TerminatedReply: Decodable, Sendable { var terminated: [Int32]? }
private struct ReportReply: Decodable, Sendable { var report: String? }
private struct PathReply: Decodable, Sendable { var path: String }
private struct ShareReply: Decodable, Sendable { var share: ShareStatus? }
private struct PortalReply: Decodable, Sendable { var portal: PortalStatus? }
private struct BenchmarkReply: Decodable, Sendable { var result: BenchmarkResult }
private struct HealthReply: Decodable, Sendable { var health: DiskHealth }
private struct Empty: Decodable, Sendable {}

struct UnmountAllResult: Decodable, Sendable {
    var unmounted: UInt32
    var failed: [String]

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        unmounted = try c.decodeIfPresent(UInt32.self, forKey: .unmounted) ?? 0
        failed = try c.decodeIfPresent([String].self, forKey: .failed) ?? []
    }
    enum CodingKeys: String, CodingKey { case unmounted, failed }
}

/// Swift 6 actor wrapping the C ABI of lumendisk-core.
///
/// Every FFI call may block (disk I/O, `mount_nfs`, `authopen`), so calls are executed on a
/// dedicated concurrent GCD queue — never on the main actor and never on the Swift cooperative
/// pool — and resumed through a continuation. Replies are copied, released with
/// `ld_free_string`, then decoded off the main actor.
actor CoreBridge {
    static let shared = CoreBridge()

    private let queue = DispatchQueue(label: "com.lumendisk.core-ffi", qos: .userInitiated, attributes: .concurrent)
    private var initialized = false
    private(set) var version: String = "—"

    // MARK: Lifecycle

    func initialize(supportDirectory: URL) async throws {
        guard !initialized else { return }
        try? FileManager.default.createDirectory(at: supportDirectory, withIntermediateDirectories: true)
        let path = supportDirectory.path
        let data = try await call { ld_init(path) }
        _ = try Self.decode(Empty.self, from: data)
        initialized = true
        if let v = try? Self.decode(VersionReply.self, from: await call { ld_version() }).version {
            version = v
        }
    }

    // MARK: Volumes

    func listVolumes() async throws -> [VolumeStatus] {
        try await checked(VolumesReply.self) { ld_list_volumes() }.volumes
    }

    func inspect(path: String, fd: Int32? = nil) async throws -> SourceInspection {
        let json = try Self.encode(SourceRequest(path: path, partition: nil, fd: fd))
        return try await checked(SourceInspection.self) { ld_inspect_source(json) }
    }

    func mount(_ request: MountRequest) async throws -> VolumeStatus {
        let json = try Self.encode(request)
        return try await checked(VolumeReply.self) { ld_mount(json) }.volume
    }

    func unmount(volumeID: String, force: Bool = false) async throws {
        _ = try await checked(Empty.self) { ld_unmount(volumeID, force) }
    }

    func unmountAll() async throws -> UnmountAllResult {
        try await checked(UnmountAllResult.self) { ld_unmount_all() }
    }

    // MARK: Shadow Mount

    func shadowChanges(volumeID: String) async throws -> [ShadowChange] {
        try await checked(ChangesReply.self) { ld_shadow_changes(volumeID) }.changes
    }

    @discardableResult
    func shadowCommit(volumeID: String) async throws -> UInt64 {
        try await checked(AppliedReply.self) { ld_shadow_commit(volumeID) }.applied ?? 0
    }

    @discardableResult
    func shadowRollback(volumeID: String) async throws -> UInt64 {
        try await checked(DiscardedReply.self) { ld_shadow_rollback(volumeID) }.discarded ?? 0
    }

    /// Every saved Shadow session, mounted or not. May read the disks to verify fingerprints.
    func shadowSessions() async throws -> [ShadowSession] {
        try await checked(SessionsReply.self) { ld_shadow_sessions() }.sessions ?? []
    }

    /// Deletes a saved session and its pending changes (only the not-yet-applied steps of an
    /// interrupted commit).
    @discardableResult
    func discardShadowSession(key: String) async throws -> UInt64 {
        try await checked(DiscardedReply.self) { ld_shadow_discard_session(key) }.discarded ?? 0
    }

    // MARK: Ghost Shield

    func ghostEntries(volumeID: String) async throws -> [GhostEntry] {
        try await checked(EntriesReply.self) { ld_ghost_entries(volumeID) }.entries
    }

    func ghostSetRules(volumeID: String, rules: GhostRules) async throws {
        let json = try Self.encode(rules)
        _ = try await checked(Empty.self) { ld_ghost_set_rules(volumeID, json) }
    }

    func ghostSetEnabled(volumeID: String, enabled: Bool) async throws {
        _ = try await checked(Empty.self) { ld_ghost_set_enabled(volumeID, enabled) }
    }

    func ghostPurge(volumeID: String) async throws {
        _ = try await checked(Empty.self) { ld_ghost_purge(volumeID) }
    }

    // MARK: UID/GID bridge

    func setUIDMap(volumeID: String, linuxUID: UInt32, linuxGID: UInt32, enabled: Bool) async throws {
        let json = try Self.encode(UIDMapUpdate(linuxUid: linuxUID, linuxGid: linuxGID, enabled: enabled))
        _ = try await checked(Empty.self) { ld_set_uid_map(volumeID, json) }
    }

    // MARK: Lock inspector

    func lockHolders(mountpoint: String) async throws -> [LockHolder] {
        try await checked(HoldersReply.self) { ld_lock_inspect(mountpoint) }.holders
    }

    @discardableResult
    func releaseLocks(mountpoint: String, pids: [Int32]) async throws -> [Int32] {
        let json = try Self.encode(pids)
        return try await checked(TerminatedReply.self) { ld_release_locks(mountpoint, json) }.terminated ?? []
    }

    // MARK: Journal doctor

    func repairSource(path: String, partition: UInt32, fd: Int32? = nil) async throws -> String {
        let json = try Self.encode(SourceRequest(path: path, partition: partition, fd: fd))
        return try await checked(ReportReply.self) { ld_repair_source(json) }.report ?? ""
    }

    func repairVolume(volumeID: String) async throws -> String {
        try await checked(ReportReply.self) { ld_repair_volume(volumeID) }.report ?? ""
    }

    // MARK: Image creation

    func createImage(_ request: CreateImageRequest) async throws -> String {
        let json = try Self.encode(request)
        return try await checked(PathReply.self) { ld_create_image(json) }.path
    }

    // MARK: - Phase 2: remote storage

    func mountRemote(_ request: RemoteMountRequest) async throws -> VolumeStatus {
        let json = try Self.encode(request)
        return try await checked(VolumeReply.self) { ld_mount_remote(json) }.volume
    }

    func probeRemote(_ request: RemoteMountRequest) async throws -> ProbeResult {
        let json = try Self.encode(request)
        return try await checked(ProbeResult.self) { ld_probe_remote(json) }
    }

    func remoteSyncNow(volumeID: String) async throws -> SyncResult {
        try await checked(SyncResult.self) { ld_remote_sync_now(volumeID) }
    }

    func inspectImageURL(_ request: ImageURLRequest) async throws -> ImageURLInspection {
        let json = try Self.encode(request)
        return try await checked(ImageURLInspection.self) { ld_inspect_image_url(json) }
    }

    func mountImageURL(_ request: ImageURLRequest) async throws -> VolumeStatus {
        let json = try Self.encode(request)
        return try await checked(VolumeReply.self) { ld_mount_image_url(json) }.volume
    }

    func shareStart(volumeID: String) async throws -> ShareStatus? {
        try await checked(ShareReply.self) { ld_share_start(volumeID) }.share
    }

    func shareStop(volumeID: String) async throws {
        _ = try await checked(Empty.self) { ld_share_stop(volumeID) }
    }

    func portalStart(volumeID: String) async throws -> PortalStatus? {
        try await checked(PortalReply.self) { ld_portal_start(volumeID) }.portal
    }

    /// Opens (or closes) the portal's unencrypted HTTP address. Off unless explicitly enabled.
    func portalSetPlainHTTP(volumeID: String, enabled: Bool) async throws -> PortalStatus? {
        try await checked(PortalReply.self) { ld_portal_set_plain_http(volumeID, enabled ? 1 : 0) }.portal
    }

    func portalStop(volumeID: String) async throws {
        _ = try await checked(Empty.self) { ld_portal_stop(volumeID) }
    }

    // MARK: - Phase 3: disk health & speed

    /// Real speed test through the mounted volume (≈3 s).
    func benchmark(volumeID: String) async throws -> BenchmarkResult {
        try await checked(BenchmarkReply.self) { ld_benchmark(volumeID) }.result
    }

    /// Hardware facts and SMART status of a physical disk or partition ("disk4s1").
    func diskHealth(bsdName: String) async throws -> DiskHealth {
        try await checked(HealthReply.self) { ld_disk_health(bsdName) }.health
    }

    // MARK: - Plumbing

    /// Runs `body` on the FFI queue, copies the returned C string and frees it.
    private nonisolated func call(_ body: @escaping @Sendable () -> UnsafeMutablePointer<CChar>?) async throws -> Data {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Data, any Error>) in
            queue.async {
                guard let pointer = body() else {
                    continuation.resume(throwing: CoreError.nullResponse)
                    return
                }
                let data = Data(bytes: pointer, count: strlen(pointer))
                ld_free_string(pointer)
                continuation.resume(returning: data)
            }
        }
    }

    private func checked<T: Decodable & Sendable>(
        _ type: T.Type,
        _ body: @escaping @Sendable () -> UnsafeMutablePointer<CChar>?
    ) async throws -> T {
        guard initialized else { throw CoreError.notInitialized }
        let data = try await call(body)
        return try Self.decode(type, from: data)
    }

    private static func decode<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let envelope: Envelope
        do {
            envelope = try decoder.decode(Envelope.self, from: data)
        } catch {
            throw CoreError.malformedResponse(L("JSON non valido", "invalid JSON"))
        }
        guard envelope.ok else {
            throw CoreError.core(envelope.error ?? L("Errore sconosciuto del motore.", "Unknown engine error."))
        }
        do {
            return try decoder.decode(type, from: data)
        } catch let DecodingError.keyNotFound(key, _) {
            throw CoreError.malformedResponse(L("campo mancante: \(key.stringValue)", "missing field: \(key.stringValue)"))
        } catch let DecodingError.typeMismatch(_, context) {
            throw CoreError.malformedResponse(L("tipo errato in ", "wrong type in ") + context.codingPath.map(\.stringValue).joined(separator: "."))
        } catch {
            throw CoreError.malformedResponse(String(describing: error))
        }
    }

    private static func encode<T: Encodable>(_ value: T) throws -> String {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        let data = try encoder.encode(value)
        return String(decoding: data, as: UTF8.self)
    }
}
