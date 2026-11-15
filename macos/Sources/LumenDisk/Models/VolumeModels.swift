import Foundation

// MARK: - Shared enums

/// Mount mode as spelled by the core (`"readWrite" | "shadow" | "readOnly"`).
enum MountMode: String, Codable, Sendable, CaseIterable, Identifiable {
    case readWrite
    case shadow
    case readOnly

    var id: String { rawValue }

    var title: String {
        switch self {
        case .readWrite: L("Lettura/Scrittura", "Read/Write")
        case .shadow: L("Shadow Mode (Sicuro)", "Shadow Mode (Safe)")
        case .readOnly: L("Sola Lettura", "Read Only")
        }
    }

    var shortTitle: String {
        switch self {
        case .readWrite: L("Lettura/Scrittura", "Read/Write")
        case .shadow: "Shadow Mode"
        case .readOnly: L("Sola Lettura", "Read Only")
        }
    }

    var symbol: String {
        switch self {
        case .readWrite: "pencil.line"
        case .shadow: "square.stack.3d.forward.dottedline"
        case .readOnly: "lock"
        }
    }

    var explanation: String {
        switch self {
        case .readWrite: L("Le modifiche vengono scritte direttamente sul disco.", "Changes are written straight to the disk.")
        case .shadow: L("Le modifiche restano in un livello sicuro finché non le applichi.", "Changes stay in a safe layer until you apply them.")
        case .readOnly: L("Il disco viene solo letto: nessuna scrittura possibile.", "The disk is only read: nothing can be written.")
        }
    }

    /// Lenient decoding: unknown spellings fall back to read-only (the safest interpretation).
    init(from decoder: any Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        switch raw.lowercased().replacingOccurrences(of: "_", with: "") {
        case "readwrite", "rw": self = .readWrite
        case "shadow": self = .shadow
        default: self = .readOnly
        }
    }
}

/// Filesystem family used for colours, icons and labels.
enum FilesystemKind: String, Sendable, CaseIterable, Identifiable {
    case ext2 = "EXT2", ext3 = "EXT3", ext4 = "EXT4"
    case fat12 = "FAT12", fat16 = "FAT16", fat32 = "FAT32"
    case exfat = "EXFAT", ntfs = "NTFS"
    case linux = "LINUX"     // DiskArbitration hint only: some ext variant, not yet inspected
    case apfs = "APFS", hfs = "HFS+"
    case unknown = "?"

    var id: String { rawValue }

    init(coreName: String?) {
        guard let name = coreName?.uppercased(), !name.isEmpty else { self = .unknown; return }
        self = FilesystemKind(rawValue: name) ?? .unknown
    }

    /// Pill text.
    var badge: String {
        switch self {
        case .exfat: "exFAT"
        case .linux: "EXT"
        case .unknown: "—"
        default: rawValue
        }
    }

    var isSupportedByCore: Bool {
        switch self {
        case .apfs, .hfs, .unknown: false
        default: true
        }
    }

    var familyName: String {
        switch self {
        case .ext2, .ext3, .ext4, .linux: "Linux"
        case .ntfs, .exfat, .fat12, .fat16, .fat32: L("Windows / Universale", "Windows / Universal")
        case .apfs, .hfs: "macOS"
        case .unknown: L("Sconosciuto", "Unknown")
        }
    }
}

// MARK: - Core JSON payloads

struct DirtyState: Codable, Sendable, Hashable {
    var state: String
    var reason: String?

    var isDirty: Bool { state.caseInsensitiveCompare("Dirty") == .orderedSame }
    static let clean = DirtyState(state: "Clean", reason: nil)
}

struct GhostRules: Codable, Sendable, Hashable {
    var dsStore: Bool = true
    var appleDouble: Bool = true
    var spotlight: Bool = true
    var trashes: Bool = true
    var fseventsd: Bool = true
    var other: Bool = true

    static let all = GhostRules()

    init() {}

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        dsStore = try c.decodeIfPresent(Bool.self, forKey: .dsStore) ?? true
        appleDouble = try c.decodeIfPresent(Bool.self, forKey: .appleDouble) ?? true
        spotlight = try c.decodeIfPresent(Bool.self, forKey: .spotlight) ?? true
        trashes = try c.decodeIfPresent(Bool.self, forKey: .trashes) ?? true
        fseventsd = try c.decodeIfPresent(Bool.self, forKey: .fseventsd) ?? true
        other = try c.decodeIfPresent(Bool.self, forKey: .other) ?? true
    }
}

struct GhostStatus: Codable, Sendable, Hashable {
    var enabled: Bool = true
    var blockedCount: UInt64 = 0
    var vaultBytes: UInt64 = 0
    var rules: GhostRules = .all

    init() {}

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        enabled = try c.decodeIfPresent(Bool.self, forKey: .enabled) ?? true
        blockedCount = try c.decodeIfPresent(UInt64.self, forKey: .blockedCount) ?? 0
        vaultBytes = try c.decodeIfPresent(UInt64.self, forKey: .vaultBytes) ?? 0
        rules = try c.decodeIfPresent(GhostRules.self, forKey: .rules) ?? .all
    }
}

struct ShadowSummary: Codable, Sendable, Hashable {
    var added: UInt64 = 0
    var modified: UInt64 = 0
    var deleted: UInt64 = 0
    var pendingBytes: Int64 = 0
    // Persistent sessions (absent on older cores)
    /// Changes are journaled on this Mac and survive a crash, quit or eject.
    var persistent = false
    var sessionKey: String?
    /// The changes were restored from a previous run when the volume was mounted.
    var restored = false
    /// `"pending"` or `"committing"` (a commit started and has not finished).
    var state = ShadowSessionState.pending
    var commitApplied: UInt64 = 0
    var commitTotal: UInt64 = 0
    var commitError: String?

    var totalChanges: UInt64 { added + modified + deleted }
    var hasChanges: Bool { totalChanges > 0 }
    /// A commit started and did not finish: new writes are refused until it is retried or the
    /// remaining steps are abandoned.
    var isCommitInterrupted: Bool { state == ShadowSessionState.committing }
    var commitRemaining: UInt64 { commitTotal > commitApplied ? commitTotal - commitApplied : 0 }

    init() {}

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        added = try c.decodeIfPresent(UInt64.self, forKey: .added) ?? 0
        modified = try c.decodeIfPresent(UInt64.self, forKey: .modified) ?? 0
        deleted = try c.decodeIfPresent(UInt64.self, forKey: .deleted) ?? 0
        pendingBytes = try c.decodeIfPresent(Int64.self, forKey: .pendingBytes) ?? 0
        persistent = (try? c.decodeIfPresent(Bool.self, forKey: .persistent)) ?? false
        sessionKey = (try? c.decodeIfPresent(String.self, forKey: .sessionKey)) ?? nil
        restored = (try? c.decodeIfPresent(Bool.self, forKey: .restored)) ?? false
        state = (try? c.decodeIfPresent(String.self, forKey: .state)) ?? ShadowSessionState.pending
        commitApplied = (try? c.decodeIfPresent(UInt64.self, forKey: .commitApplied)) ?? 0
        commitTotal = (try? c.decodeIfPresent(UInt64.self, forKey: .commitTotal)) ?? 0
        commitError = (try? c.decodeIfPresent(String.self, forKey: .commitError)) ?? nil
    }
}

enum ShadowSessionState {
    static let pending = "pending"
    static let committing = "committing"
}

/// A saved Shadow session (`ld_shadow_sessions`): the pending changes of one volume, kept on this
/// Mac until they are committed or discarded, whether the volume is mounted or not.
struct ShadowSession: Codable, Sendable, Hashable, Identifiable {
    var key: String
    var name: String
    var label: String
    var fsType: String
    var uuid: String
    var sourcePath: String
    var partition: UInt32
    /// `"local"` or `"image_url"`.
    var kind: String
    var mediaUUID: String?
    var size: UInt64
    var created: Int64
    var updated: Int64
    var state: String
    var changes: UInt64
    var added: UInt64
    var modified: UInt64
    var deleted: UInt64
    var pendingBytes: Int64
    var commitApplied: UInt64
    var commitTotal: UInt64
    var commitError: String?
    /// The source path exists right now (image file or device node). For physical disks the app
    /// also checks the media UUID: see `AppModel.isConnected(_:)`.
    var connected: Bool
    var mountedVolumeID: String?
    /// nil = not verified, false = the disk was modified elsewhere (only Discard is possible).
    var fingerprintOK: Bool?
    var fingerprintProblem: String?

    var id: String { key }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        key = try c.decode(String.self, forKey: .key)
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? ""
        label = try c.decodeIfPresent(String.self, forKey: .label) ?? ""
        fsType = try c.decodeIfPresent(String.self, forKey: .fsType) ?? ""
        uuid = try c.decodeIfPresent(String.self, forKey: .uuid) ?? ""
        sourcePath = try c.decodeIfPresent(String.self, forKey: .sourcePath) ?? ""
        partition = try c.decodeIfPresent(UInt32.self, forKey: .partition) ?? 0
        kind = try c.decodeIfPresent(String.self, forKey: .kind) ?? "local"
        mediaUUID = try c.decodeIfPresent(String.self, forKey: .mediaUUID)
        size = try c.decodeIfPresent(UInt64.self, forKey: .size) ?? 0
        created = try c.decodeIfPresent(Int64.self, forKey: .created) ?? 0
        updated = try c.decodeIfPresent(Int64.self, forKey: .updated) ?? created
        state = try c.decodeIfPresent(String.self, forKey: .state) ?? ShadowSessionState.pending
        added = try c.decodeIfPresent(UInt64.self, forKey: .added) ?? 0
        modified = try c.decodeIfPresent(UInt64.self, forKey: .modified) ?? 0
        deleted = try c.decodeIfPresent(UInt64.self, forKey: .deleted) ?? 0
        changes = try c.decodeIfPresent(UInt64.self, forKey: .changes) ?? (added + modified + deleted)
        pendingBytes = try c.decodeIfPresent(Int64.self, forKey: .pendingBytes) ?? 0
        commitApplied = try c.decodeIfPresent(UInt64.self, forKey: .commitApplied) ?? 0
        commitTotal = try c.decodeIfPresent(UInt64.self, forKey: .commitTotal) ?? 0
        commitError = try c.decodeIfPresent(String.self, forKey: .commitError)
        connected = try c.decodeIfPresent(Bool.self, forKey: .connected) ?? false
        mountedVolumeID = try c.decodeIfPresent(String.self, forKey: .mountedVolumeID)
        fingerprintOK = try c.decodeIfPresent(Bool.self, forKey: .fingerprintOK)
        fingerprintProblem = try c.decodeIfPresent(String.self, forKey: .fingerprintProblem)
    }

    // Raw values are the keys after `.convertFromSnakeCase` ("media_uuid" → "mediaUuid").
    enum CodingKeys: String, CodingKey {
        case key, name, label, fsType, uuid, sourcePath, partition, kind
        case mediaUUID = "mediaUuid"
        case size, created, updated, state, changes, added, modified, deleted, pendingBytes
        case commitApplied, commitTotal, commitError, connected
        case mountedVolumeID = "mountedVolumeId"
        case fingerprintOK = "fingerprintOk"
        case fingerprintProblem
    }

    // MARK: Derived

    var displayName: String {
        if !name.isEmpty { return name }
        if !label.isEmpty { return label }
        return sourceFileName
    }

    var filesystem: FilesystemKind { FilesystemKind(coreName: fsType) }
    var isImageURL: Bool { kind == "image_url" }
    /// Physical partition (`/dev/diskNsM`), as opposed to an image file.
    var deviceBSDName: String? { isImageURL ? nil : DeviceNode.bsdName(fromPath: sourcePath) }
    var isPhysical: Bool { deviceBSDName != nil }
    var isCommitInterrupted: Bool { state == ShadowSessionState.committing }
    /// The disk was modified elsewhere since the session started: only Discard is possible.
    var isStale: Bool { fingerprintOK == false }
    var changeCount: UInt64 { changes > 0 ? changes : added + modified + deleted }
    var commitRemaining: UInt64 { commitTotal > commitApplied ? commitTotal - commitApplied : 0 }
    var updatedDate: Date { Date(timeIntervalSince1970: TimeInterval(updated > 0 ? updated : created)) }
    var sourceFileName: String {
        if isImageURL, let url = URL(string: sourcePath), !url.lastPathComponent.isEmpty { return url.lastPathComponent }
        return URL(fileURLWithPath: sourcePath).lastPathComponent
    }

    /// What makes a recovery prompt worth showing again in the same run. The change count is
    /// left out on purpose: it only moves when the user edits the volume in this run.
    var promptSignature: String { "\(state)|\(isStale)|\(commitApplied)" }
}

struct UIDMapStatus: Codable, Sendable, Hashable {
    var enabled: Bool = false
    var macUid: UInt32 = 501
    var macGid: UInt32 = 20
    var linuxUid: UInt32 = 1000
    var linuxGid: UInt32 = 1000
}

struct VolumeStatus: Codable, Sendable, Hashable, Identifiable {
    var id: String
    var name: String
    var sourcePath: String
    var partition: UInt32
    var fsType: String
    var label: String
    var uuid: String
    var mode: MountMode
    var mountpoint: String
    var port: UInt16
    var readOnly: Bool
    var forcedReadOnlyReason: String?
    var dirty: DirtyState
    var totalBytes: UInt64
    var freeBytes: UInt64
    var ghost: GhostStatus
    var shadow: ShadowSummary?
    var uidMap: UIDMapStatus
    var mountedAt: Int64
    // Phase 2 (absent on older cores)
    var kind: VolumeKind
    var remote: RemoteStatus?
    var share: ShareStatus?
    var portal: PortalStatus?
    var journal: JournalStatus?
    // Phase 3: BSD name ("disk4s1") when mounted from a physical device.
    var device: String?

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? ""
        sourcePath = try c.decodeIfPresent(String.self, forKey: .sourcePath) ?? ""
        partition = try c.decodeIfPresent(UInt32.self, forKey: .partition) ?? 0
        fsType = try c.decodeIfPresent(String.self, forKey: .fsType) ?? ""
        label = try c.decodeIfPresent(String.self, forKey: .label) ?? ""
        uuid = try c.decodeIfPresent(String.self, forKey: .uuid) ?? ""
        mode = try c.decodeIfPresent(MountMode.self, forKey: .mode) ?? .readOnly
        mountpoint = try c.decodeIfPresent(String.self, forKey: .mountpoint) ?? ""
        port = try c.decodeIfPresent(UInt16.self, forKey: .port) ?? 0
        readOnly = try c.decodeIfPresent(Bool.self, forKey: .readOnly) ?? (mode == .readOnly)
        forcedReadOnlyReason = try c.decodeIfPresent(String.self, forKey: .forcedReadOnlyReason)
        dirty = try c.decodeIfPresent(DirtyState.self, forKey: .dirty) ?? .clean
        totalBytes = try c.decodeIfPresent(UInt64.self, forKey: .totalBytes) ?? 0
        freeBytes = try c.decodeIfPresent(UInt64.self, forKey: .freeBytes) ?? 0
        ghost = try c.decodeIfPresent(GhostStatus.self, forKey: .ghost) ?? GhostStatus()
        shadow = try c.decodeIfPresent(ShadowSummary.self, forKey: .shadow)
        uidMap = try c.decodeIfPresent(UIDMapStatus.self, forKey: .uidMap) ?? UIDMapStatus()
        mountedAt = try c.decodeIfPresent(Int64.self, forKey: .mountedAt) ?? 0
        remote = try? c.decodeIfPresent(RemoteStatus.self, forKey: .remote)
        share = try? c.decodeIfPresent(ShareStatus.self, forKey: .share)
        portal = try? c.decodeIfPresent(PortalStatus.self, forKey: .portal)
        journal = try? c.decodeIfPresent(JournalStatus.self, forKey: .journal)
        device = (try? c.decodeIfPresent(String.self, forKey: .device))
            .flatMap { $0 }
            .flatMap { DeviceNode.bsdName(fromPath: $0.hasPrefix("/dev/") ? $0 : "/dev/" + $0) }
        kind = (try? c.decodeIfPresent(VolumeKind.self, forKey: .kind))
            ?? VolumeKind.infer(sourcePath: sourcePath, hasRemote: remote != nil)
    }

    // MARK: Derived

    var displayName: String {
        if !name.isEmpty { return name }
        if !label.isEmpty { return label }
        return URL(fileURLWithPath: mountpoint).lastPathComponent
    }

    var filesystem: FilesystemKind { FilesystemKind(coreName: fsType) }
    var isRemote: Bool { kind.isRemote }
    /// Remote volumes whose changes are uploaded later (SFTP, WebDAV, LAN). Remote images keep
    /// writes local, so they never have an upload queue.
    var hasSyncQueue: Bool { kind == .sftp || kind == .webdav || kind == .lan }
    var remoteURL: String { remote?.url ?? sourcePath }
    /// Host part of the remote URL ("raspberrypi.local").
    var remoteHost: String? { URL(string: remoteURL)?.host() }
    var usedBytes: UInt64 { totalBytes > freeBytes ? totalBytes - freeBytes : 0 }
    var shadowPendingBytes: UInt64 {
        guard mode == .shadow, let shadow, shadow.pendingBytes > 0 else { return 0 }
        return UInt64(shadow.pendingBytes)
    }
    /// Remote images always report a forced read-only reason (writes stay in the local Shadow
    /// layer by design): that is not something to repair.
    var needsRepair: Bool { dirty.isDirty || (forcedReadOnlyReason != nil && kind != .imageURL) }
    var hasPendingShadowChanges: Bool { mode == .shadow && (shadow?.hasChanges ?? false) }
    /// A Shadow commit stopped half-way (crash, unplugged disk, I/O error).
    var isShadowCommitInterrupted: Bool { mode == .shadow && (shadow?.isCommitInterrupted ?? false) }
    /// Pending Shadow changes survive an eject or quit (kept on this Mac by the core).
    var keepsShadowChanges: Bool { mode == .shadow && (shadow?.persistent ?? false) }

    /// `diskNsM` when the source is a physical device node, else nil.
    var deviceBSDName: String? {
        guard kind == .local else { return nil }
        return device ?? DeviceNode.bsdName(fromPath: sourcePath)
    }
    var isPhysical: Bool { deviceBSDName != nil }
    var sourceFileName: String {
        if isRemote, let url = URL(string: remoteURL), !url.lastPathComponent.isEmpty, url.lastPathComponent != "/" {
            return url.lastPathComponent
        }
        return URL(fileURLWithPath: sourcePath).lastPathComponent
    }

    /// Folder name used as mountpoint, reused when remounting.
    var mountFolderName: String { URL(fileURLWithPath: mountpoint).lastPathComponent }
}

struct PartitionInfo: Codable, Sendable, Hashable, Identifiable {
    var index: UInt32
    var start: UInt64
    var size: UInt64
    var scheme: String
    var typeName: String
    var typeId: String
    var name: String
    var fsType: String?
    var label: String?
    var uuid: String?
    var dirty: DirtyState?
    var totalBytes: UInt64?
    var freeBytes: UInt64?

    var id: UInt32 { index }
    var filesystem: FilesystemKind { FilesystemKind(coreName: fsType) }
    var isMountable: Bool { fsType != nil && filesystem.isSupportedByCore }

    var displayName: String {
        if let label, !label.isEmpty { return label }
        if !name.isEmpty { return name }
        return index == 0 ? L("Volume intero", "Whole volume") : L("Partizione \(index)", "Partition \(index)")
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        index = try c.decodeIfPresent(UInt32.self, forKey: .index) ?? 0
        start = try c.decodeIfPresent(UInt64.self, forKey: .start) ?? 0
        size = try c.decodeIfPresent(UInt64.self, forKey: .size) ?? 0
        scheme = try c.decodeIfPresent(String.self, forKey: .scheme) ?? "NONE"
        typeName = try c.decodeIfPresent(String.self, forKey: .typeName) ?? ""
        typeId = try c.decodeIfPresent(String.self, forKey: .typeId) ?? ""
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? ""
        fsType = try c.decodeIfPresent(String.self, forKey: .fsType)
        label = try c.decodeIfPresent(String.self, forKey: .label)
        uuid = try c.decodeIfPresent(String.self, forKey: .uuid)
        dirty = try c.decodeIfPresent(DirtyState.self, forKey: .dirty)
        totalBytes = try c.decodeIfPresent(UInt64.self, forKey: .totalBytes)
        freeBytes = try c.decodeIfPresent(UInt64.self, forKey: .freeBytes)
    }
}

struct SourceInspection: Codable, Sendable, Hashable {
    var path: String
    var size: UInt64
    var partitions: [PartitionInfo]
}

enum ShadowChangeKind: String, Codable, Sendable, CaseIterable, Identifiable {
    case added, modified, deleted
    var id: String { rawValue }

    var title: String {
        switch self {
        case .added: L("Aggiunto", "Added")
        case .modified: L("Modificato", "Modified")
        case .deleted: L("Eliminato", "Deleted")
        }
    }

    var pluralTitle: String {
        switch self {
        case .added: L("Aggiunti", "Added")
        case .modified: L("Modificati", "Modified")
        case .deleted: L("Eliminati", "Deleted")
        }
    }

    var symbol: String {
        switch self {
        case .added: "plus.circle.fill"
        case .modified: "pencil.circle.fill"
        case .deleted: "minus.circle.fill"
        }
    }
}

struct ShadowChange: Codable, Sendable, Hashable, Identifiable {
    var path: String
    var kind: ShadowChangeKind
    var isDir: Bool
    var oldSize: UInt64
    var newSize: UInt64
    var delta: Int64

    var id: String { kind.rawValue + ":" + path }
    var fileName: String { (path as NSString).lastPathComponent }
    var parentPath: String {
        let parent = (path as NSString).deletingLastPathComponent
        return parent.isEmpty ? "/" : parent
    }
}

enum GhostCategory: String, Codable, Sendable, CaseIterable, Identifiable {
    case dsStore = "ds_store"
    case appleDouble = "apple_double"
    case spotlight
    case trashes
    case fseventsd
    case other

    var id: String { rawValue }

    init(from decoder: any Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = GhostCategory(rawValue: raw) ?? .other
    }

    var title: String {
        switch self {
        case .dsStore: ".DS_Store"
        case .appleDouble: L("File AppleDouble (._*)", "AppleDouble files (._*)")
        case .spotlight: L("Indice Spotlight", "Spotlight index")
        case .trashes: L("Cestino (.Trashes)", "Trash (.Trashes)")
        case .fseventsd: L("Registro eventi (.fseventsd)", "Event log (.fseventsd)")
        case .other: L("Altri metadati macOS", "Other macOS metadata")
        }
    }

    var subtitle: String {
        switch self {
        case .dsStore: L("Preferenze di visualizzazione delle cartelle del Finder", "Finder folder view preferences")
        case .appleDouble: L("Attributi estesi e fork risorse duplicati come ._nomefile", "Extended attributes and resource forks copied as ._filename")
        case .spotlight: L("Database di indicizzazione .Spotlight-V100", "The .Spotlight-V100 search index")
        case .trashes: L("File spostati nel Cestino dal Finder", "Files moved to the Trash by Finder")
        case .fseventsd: L("Log delle modifiche al file system", "File system change log")
        case .other: L(".TemporaryItems, .VolumeIcon.icns e simili", ".TemporaryItems, .VolumeIcon.icns and the like")
        }
    }

    var symbol: String {
        switch self {
        case .dsStore: "folder.badge.gearshape"
        case .appleDouble: "doc.on.doc"
        case .spotlight: "magnifyingglass"
        case .trashes: "trash"
        case .fseventsd: "clock.arrow.circlepath"
        case .other: "ellipsis.circle"
        }
    }

    var ruleKeyPath: WritableKeyPath<GhostRules, Bool> {
        switch self {
        case .dsStore: \.dsStore
        case .appleDouble: \.appleDouble
        case .spotlight: \.spotlight
        case .trashes: \.trashes
        case .fseventsd: \.fseventsd
        case .other: \.other
        }
    }
}

struct GhostEntry: Codable, Sendable, Hashable, Identifiable {
    var path: String
    var size: UInt64
    var category: GhostCategory
    var id: String { path }
}

struct LockHolder: Codable, Sendable, Hashable, Identifiable {
    var pid: Int32
    var name: String
    var executable: String?
    var files: [String]
    var id: Int32 { pid }
}

enum ImageFilesystem: String, Codable, Sendable, CaseIterable, Identifiable {
    case ext4 = "EXT4", ntfs = "NTFS", fat32 = "FAT32", exfat = "EXFAT"
    var id: String { rawValue }
    var kind: FilesystemKind {
        switch self {
        case .ext4: .ext4
        case .ntfs: .ntfs
        case .fat32: .fat32
        case .exfat: .exfat
        }
    }
    var minimumSizeMB: UInt32 {
        switch self {
        case .fat32: 64
        case .ext4: 16
        case .ntfs: 16
        case .exfat: 8
        }
    }
    var maxLabelLength: Int {
        switch self {
        case .fat32: 11
        case .exfat: 15
        case .ntfs: 32
        case .ext4: 16
        }
    }
    var fileExtension: String {
        switch self {
        case .ext4: "ext4"
        case .ntfs: "ntfs"
        case .fat32: "fat32"
        case .exfat: "img"
        }
    }
}

// MARK: - Requests

struct UIDMapRequest: Encodable, Sendable, Hashable {
    var enabled: Bool
    var linuxUid: UInt32?
    var linuxGid: UInt32?

    static let automatic = UIDMapRequest(enabled: true, linuxUid: nil, linuxGid: nil)

    func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(enabled, forKey: .enabled)
        try c.encode(linuxUid, forKey: .linuxUid)   // explicit null = auto-detect
        try c.encode(linuxGid, forKey: .linuxGid)
    }
    enum CodingKeys: String, CodingKey { case enabled, linuxUid, linuxGid }
}

struct MountRequest: Encodable, Sendable, Hashable {
    var path: String
    var partition: UInt32
    var mode: MountMode
    var ghostShield: Bool = true
    var uidMap: UIDMapRequest = .automatic
    var mountRoot: String? = AppPaths.defaultMountRoot
    var name: String? = nil
    var fd: Int32? = nil
    var volumeIcon: String? = AppPaths.volumeIconPath
    /// DiskArbitration media UUID of a physical partition, stored in its Shadow session so the
    /// disk can be recognised when it is plugged in again.
    var mediaUUID: String? = nil
    /// `ShadowSession.key` to resume: the core refuses the mount if the source doesn't hold it.
    var expectSession: String? = nil

    func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(path, forKey: .path)
        try c.encode(partition, forKey: .partition)
        try c.encode(mode.rawValue, forKey: .mode)
        try c.encode(ghostShield, forKey: .ghostShield)
        try c.encode(uidMap, forKey: .uidMap)
        try c.encode(mountRoot, forKey: .mountRoot)
        try c.encode(name, forKey: .name)
        try c.encode(fd, forKey: .fd)
        try c.encode(volumeIcon, forKey: .volumeIcon)
        // Only sent when set, so older cores never see them.
        try c.encodeIfPresent(mediaUUID, forKey: .mediaUUID)
        try c.encodeIfPresent(expectSession, forKey: .expectSession)
    }
    // `.convertToSnakeCase` turns these into "media_uuid" / "expect_session".
    enum CodingKeys: String, CodingKey {
        case path, partition, mode, ghostShield, uidMap, mountRoot, name, fd, volumeIcon
        case mediaUUID = "mediaUuid"
        case expectSession
    }
}

struct SourceRequest: Encodable, Sendable {
    var path: String
    var partition: UInt32?
    var fd: Int32? = nil

    func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(path, forKey: .path)
        if let partition { try c.encode(partition, forKey: .partition) }
        try c.encode(fd, forKey: .fd)
    }
    enum CodingKeys: String, CodingKey { case path, partition, fd }
}

struct UIDMapUpdate: Encodable, Sendable {
    var linuxUid: UInt32
    var linuxGid: UInt32
    var enabled: Bool
}

struct CreateImageRequest: Encodable, Sendable {
    var path: String
    var fs: ImageFilesystem
    var sizeMb: UInt32
    var label: String
}

// MARK: - Helpers

enum DeviceNode {
    /// `/dev/disk4s1` or `/dev/rdisk4s1` → `disk4s1`.
    static func bsdName(fromPath path: String) -> String? {
        guard path.hasPrefix("/dev/") else { return nil }
        var name = String(path.dropFirst(5))
        if name.hasPrefix("rdisk") { name.removeFirst() }
        return name.hasPrefix("disk") ? name : nil
    }

    /// `disk4s1` → `disk4`.
    static func wholeDisk(of bsdName: String) -> String {
        guard bsdName.hasPrefix("disk") else { return bsdName }
        let rest = bsdName.dropFirst(4)
        let digits = rest.prefix { $0.isNumber }
        return "disk" + digits
    }
}

enum ByteFormat {
    static func string(_ bytes: UInt64) -> String {
        string(Int64(clamping: bytes))
    }

    static func string(_ bytes: Int64) -> String {
        // Foundation spells 0 as "Zero kB"; a plain number reads better next to other sizes.
        if bytes == 0 { return L("0 byte", "0 bytes") }
        return bytes.formatted(.byteCount(style: .file).locale(L10n.locale))
    }

    /// "+1,2 MB" / "−3 byte" / "0 byte".
    static func delta(_ bytes: Int64) -> String {
        if bytes == 0 { return string(Int64(0)) }
        let magnitude = string(bytes.magnitude)
        return (bytes > 0 ? "+" : "−") + magnitude
    }
}
