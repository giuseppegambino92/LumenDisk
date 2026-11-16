import Foundation

// Forge (LumenDisk 1.1): the JSON exchanged with the core through ld_forge_* (see the forge
// section of core/include/lumendisk_ffi.h and core/src/forge/plan.rs). Field names are the ABI,
// so every type spells its keys explicitly and is decoded with a plain JSONDecoder (no key
// strategy): a key conversion would also rewrite the keys of free-form result objects.

// MARK: - Generic JSON (job results, containers)

/// Any JSON value. Job results differ per operation and are read field by field.
enum JSONValue: Sendable, Hashable, Codable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    init(from decoder: any Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null; return }
        if let b = try? c.decode(Bool.self) { self = .bool(b); return }
        if let n = try? c.decode(Double.self) { self = .number(n); return }
        if let s = try? c.decode(String.self) { self = .string(s); return }
        if let a = try? c.decode([JSONValue].self) { self = .array(a); return }
        self = .object(try c.decode([String: JSONValue].self))
    }

    func encode(to encoder: any Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .number(let n):
            if n.rounded() == n, abs(n) < 9.0e15 { try c.encode(Int64(n)) } else { try c.encode(n) }
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }

    subscript(key: String) -> JSONValue? {
        if case .object(let o) = self { return o[key] }
        return nil
    }

    var isNull: Bool { self == .null }
    var stringValue: String? { if case .string(let s) = self { return s }; return nil }
    var doubleValue: Double? { if case .number(let n) = self { return n }; return nil }
    var uint64Value: UInt64? { doubleValue.flatMap { $0 >= 0 ? UInt64(exactly: $0.rounded()) : nil } }
    var boolValue: Bool? { if case .bool(let b) = self { return b }; return nil }
    var arrayValue: [JSONValue]? { if case .array(let a) = self { return a }; return nil }
    var objectValue: [String: JSONValue]? { if case .object(let o) = self { return o }; return nil }

    /// Re-decode this value as a typed model.
    func decoded<T: Decodable>(as type: T.Type) -> T? {
        guard let data = try? JSONEncoder().encode(self) else { return nil }
        return try? JSONDecoder().decode(type, from: data)
    }
}

enum ForgeJSON {
    static func decode<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
        try JSONDecoder().decode(type, from: data)
    }

    static func decode<T: Decodable>(_ type: T.Type, from text: String) throws -> T {
        try decode(type, from: Data(text.utf8))
    }

    /// Requests are sent with sorted keys: stable JSON for logs and golden tests.
    static func encode<T: Encodable>(_ value: T) throws -> String {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return String(decoding: try encoder.encode(value), as: UTF8.self)
    }
}

extension KeyedDecodingContainer {
    /// Tolerant read: a missing key, a null or a value of another type gives `fallback`.
    func value<T: Decodable>(_ key: Key, _ fallback: T) -> T {
        ((try? decodeIfPresent(T.self, forKey: key)) ?? nil) ?? fallback
    }

    func optional<T: Decodable>(_ key: Key) -> T? {
        (try? decodeIfPresent(T.self, forKey: key)) ?? nil
    }
}

// MARK: - Enumerations shared by requests and replies

/// Every filesystem the core can create (`TargetFs`).
enum ForgeFS: String, CaseIterable, Codable, Sendable, Identifiable {
    case ext2, ext3, ext4, fat16, fat32, exfat, ntfs, btrfs, xfs, f2fs, ufs2

    var id: String { rawValue }
}

enum ForgeScheme: String, CaseIterable, Codable, Sendable, Identifiable {
    case gpt, mbr, apm, none
    var id: String { rawValue }
}

enum ForgeWriteMode: String, CaseIterable, Codable, Sendable, Identifiable {
    case auto, dd, iso, windows
    var id: String { rawValue }
}

enum WindowsLayout: String, CaseIterable, Codable, Sendable, Identifiable {
    case fat32Split = "fat32_split"
    case ntfsUefi = "ntfs_uefi"
    var id: String { rawValue }
}

enum BootTarget: String, CaseIterable, Codable, Sendable, Identifiable {
    case uefi, bios, both
    var id: String { rawValue }
}

enum WipeMethod: String, CaseIterable, Codable, Sendable, Identifiable {
    case zeros, random, dod3, dod7, gutmann
    var id: String { rawValue }
}

enum DoctorAction: String, CaseIterable, Codable, Sendable, Identifiable {
    case check, repair
    case convertToGpt = "convert_to_gpt"
    case convertToMbr = "convert_to_mbr"
    var id: String { rawValue }
}

enum HashAlgorithm: String, CaseIterable, Codable, Sendable, Identifiable {
    case sha256, sha1, md5
    var id: String { rawValue }

    var title: String {
        switch self {
        case .sha256: "SHA-256"
        case .sha1: "SHA-1"
        case .md5: "MD5"
        }
    }

    /// Length of the hex digest.
    var hexLength: Int {
        switch self {
        case .sha256: 64
        case .sha1: 40
        case .md5: 32
        }
    }
}

// MARK: - Requests (plan.rs)

enum ForgeTarget: Encodable, Sendable, Equatable {
    case disk(bsd: String)
    case file(path: String, size: UInt64?)

    private enum Keys: String, CodingKey { case kind, bsd, path, size }

    func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        switch self {
        case .disk(let bsd):
            try c.encode("disk", forKey: .kind)
            try c.encode(bsd, forKey: .bsd)
        case .file(let path, let size):
            try c.encode("file", forKey: .kind)
            try c.encode(path, forKey: .path)
            try c.encode(size, forKey: .size)   // explicit null: grow to the image's size
        }
    }
}

struct PartSpec: Encodable, Sendable, Equatable {
    var fs: ForgeFS
    var label: String
    var clusterSize: UInt32?
    /// Bytes; nil = the rest of the disk (last partition only).
    var size: UInt64?
    var name: String?

    enum CodingKeys: String, CodingKey {
        case fs, label, size, name
        case clusterSize = "cluster_size"
    }
}

struct FormatRequest: Encodable, Sendable, Equatable {
    var target: ForgeTarget
    var scheme: ForgeScheme
    var partitions: [PartSpec]
    var clearSignatures = true

    enum CodingKeys: String, CodingKey {
        case target, scheme, partitions
        case clearSignatures = "clear_signatures"
    }
}

struct LocalAccount: Encodable, Sendable, Equatable {
    var name: String
    var password: String
}

struct WindowsOptions: Encodable, Sendable, Equatable {
    var layout: WindowsLayout?
    var boot: BootTarget?
    var label: String?
    var bypassHardwareChecks = false
    var localAccount: LocalAccount?
    var skipPrivacyQuestions = false
    var locale: String?
    var timeZone: String?
    var customAnswerFile: String?

    enum CodingKeys: String, CodingKey {
        case layout, boot, label, locale
        case bypassHardwareChecks = "bypass_hardware_checks"
        case localAccount = "local_account"
        case skipPrivacyQuestions = "skip_privacy_questions"
        case timeZone = "time_zone"
        case customAnswerFile = "custom_answer_file"
    }
}

struct LinuxOptions: Encodable, Sendable, Equatable {
    var persistenceSize: UInt64?
    var fs: ForgeFS?
    var boot: BootTarget?
    var label: String?

    enum CodingKeys: String, CodingKey {
        case fs, boot, label
        case persistenceSize = "persistence_size"
    }
}

struct WifiOptions: Encodable, Sendable, Equatable {
    var ssid: String
    var password: String
    var country: String
    var hidden: Bool
}

struct RpiOptions: Encodable, Sendable, Equatable {
    var hostname: String?
    var user: String?
    var password: String?
    var ssh = false
    var sshAuthorizedKeys: String?
    var wifi: WifiOptions?
    var locale: String?
    var timeZone: String?
    var keyboardLayout: String?

    enum CodingKeys: String, CodingKey {
        case hostname, user, password, ssh, wifi, locale
        case sshAuthorizedKeys = "ssh_authorized_keys"
        case timeZone = "time_zone"
        case keyboardLayout = "keyboard_layout"
    }
}

struct FlashRequest: Encodable, Sendable, Equatable {
    var source: String
    var target: ForgeTarget
    var mode: ForgeWriteMode = .auto
    var verify = true
    var expectHash: String?
    var windows: WindowsOptions?
    var linux: LinuxOptions?
    var rpi: RpiOptions?
    var eject = true

    enum CodingKeys: String, CodingKey {
        case source, target, mode, verify, windows, linux, rpi, eject
        case expectHash = "expect_hash"
    }
}

struct WipeRequest: Encodable, Sendable, Equatable {
    var target: ForgeTarget
    var method: WipeMethod
    var verify = true
}

struct HashRequest: Encodable, Sendable, Equatable {
    var source: String
    var algorithms: [HashAlgorithm]
    var decompressed = false
    var offset: UInt64?
    var length: UInt64?
}

struct DoctorRequest: Encodable, Sendable, Equatable {
    var target: ForgeTarget
    var action: DoctorAction
}

/// `plan::Request`, tagged by "op".
enum ForgeRequest: Encodable, Sendable, Equatable {
    case format(FormatRequest)
    case flash(FlashRequest)
    case wipe(WipeRequest)
    case hash(HashRequest)
    case doctor(DoctorRequest)

    var op: String {
        switch self {
        case .format: "format"
        case .flash: "flash"
        case .wipe: "wipe"
        case .hash: "hash"
        case .doctor: "doctor"
        }
    }

    private enum OpKey: String, CodingKey { case op }

    func encode(to encoder: any Encoder) throws {
        switch self {
        case .format(let r): try r.encode(to: encoder)
        case .flash(let r): try r.encode(to: encoder)
        case .wipe(let r): try r.encode(to: encoder)
        case .hash(let r): try r.encode(to: encoder)
        case .doctor(let r): try r.encode(to: encoder)
        }
        var c = encoder.container(keyedBy: OpKey.self)
        try c.encode(op, forKey: .op)
    }

    func json() throws -> String { try ForgeJSON.encode(self) }

    /// The disk a request writes to (or reads, for doctor checks / disk hashes).
    var diskBSD: String? {
        let target: ForgeTarget? = switch self {
        case .format(let r): r.target
        case .flash(let r): r.target
        case .wipe(let r): r.target
        case .doctor(let r): r.target
        case .hash(let r): r.source.hasPrefix("disk") ? .disk(bsd: r.source) : nil
        }
        if case .disk(let bsd)? = target { return bsd }
        return nil
    }
}

// MARK: - Candidate disks (target.rs DiskInfo)

struct ForgeDisk: Decodable, Sendable, Hashable, Identifiable {
    var bsd: String
    var mediaName: String
    var size: UInt64
    var blockSize: UInt32
    var isInternal: Bool
    var removable: Bool
    var ejectable: Bool
    var whole: Bool
    var writable: Bool
    var diskImage: Bool
    var busProtocol: String
    var boot: Bool
    var mounted: [String]
    var allowed: Bool
    var refusal: String?

    var id: String { bsd }

    enum CodingKeys: String, CodingKey {
        case bsd, size, removable, ejectable, whole, writable, boot, mounted, allowed, refusal
        case mediaName = "media_name"
        case blockSize = "block_size"
        case isInternal = "internal"
        case diskImage = "disk_image"
        case busProtocol = "protocol"
    }

    init(bsd: String, mediaName: String = "", size: UInt64 = 0, blockSize: UInt32 = 512, isInternal: Bool = false,
         removable: Bool = false, ejectable: Bool = false, whole: Bool = true, writable: Bool = true, diskImage: Bool = false,
         busProtocol: String = "", boot: Bool = false, mounted: [String] = [], allowed: Bool = false, refusal: String? = nil) {
        self.bsd = bsd; self.mediaName = mediaName; self.size = size; self.blockSize = blockSize
        self.isInternal = isInternal; self.removable = removable; self.ejectable = ejectable; self.whole = whole
        self.writable = writable; self.diskImage = diskImage; self.busProtocol = busProtocol; self.boot = boot
        self.mounted = mounted; self.allowed = allowed; self.refusal = refusal
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        bsd = try c.decode(String.self, forKey: .bsd)
        mediaName = c.value(.mediaName, "")
        size = c.value(.size, 0)
        blockSize = c.value(.blockSize, 512)
        isInternal = c.value(.isInternal, false)
        removable = c.value(.removable, false)
        ejectable = c.value(.ejectable, false)
        whole = c.value(.whole, false)
        writable = c.value(.writable, false)
        diskImage = c.value(.diskImage, false)
        busProtocol = c.value(.busProtocol, "")
        boot = c.value(.boot, false)
        mounted = c.value(.mounted, [])
        allowed = c.value(.allowed, false)
        refusal = c.optional(.refusal)
    }

    /// What the user recognises the disk by.
    var title: String {
        let name = mediaName.trimmingCharacters(in: .whitespaces)
        if diskImage && (name.isEmpty || name == "Disk Image") { return L("Immagine disco collegata", "Attached disk image") }
        return name.isEmpty ? bsd : name
    }

    /// Mounted volume names (the core reports mountpoints).
    var volumeNames: [String] { mounted.map { ($0 as NSString).lastPathComponent } }

    var symbol: String {
        if diskImage { return "opticaldiscdrive" }
        if busProtocol.localizedCaseInsensitiveContains("secure digital") { return "sdcard" }
        if isInternal { return "internaldrive" }
        return "externaldrive"
    }
}

struct ForgeDisksReply: Decodable, Sendable {
    var disks: [ForgeDisk]
}

// MARK: - Image inspection (image.rs Inspection)

struct ImagePartition: Decodable, Sendable, Hashable {
    var index: UInt32
    var start: UInt64
    var size: UInt64
    var scheme: String
    var typeName: String
    var name: String

    enum CodingKeys: String, CodingKey {
        case index, start, size, scheme, name
        case typeName = "type_name"
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        index = c.value(.index, 0)
        start = c.value(.start, 0)
        size = c.value(.size, 0)
        scheme = c.value(.scheme, "")
        typeName = c.value(.typeName, "")
        name = c.value(.name, "")
    }
}

struct IsoInfo: Decodable, Sendable, Hashable {
    var label: String
    var size: UInt64
    var joliet: Bool
    var udf: Bool
    var elTorito: Bool
    var efiBoot: Bool
    var hybridTables: [String]
    var hybrid: Bool

    enum CodingKeys: String, CodingKey {
        case label, size, joliet, udf, hybrid
        case elTorito = "el_torito"
        case efiBoot = "efi_boot"
        case hybridTables = "hybrid_tables"
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        label = c.value(.label, "")
        size = c.value(.size, 0)
        joliet = c.value(.joliet, false)
        udf = c.value(.udf, false)
        elTorito = c.value(.elTorito, false)
        efiBoot = c.value(.efiBoot, false)
        hybridTables = c.value(.hybridTables, [])
        hybrid = c.value(.hybrid, false)
    }
}

struct WindowsInfo: Decodable, Sendable, Hashable {
    var version: String
    var arch: String
    var installImage: String
    var installImageSize: UInt64
    var editions: [String]
    var build: UInt32?
    var server: Bool
    var languages: [String]
    var imageFormat: String
    var needsSplit: Bool
    var splitBlocker: String?
    var biosBoot: Bool
    var uefiBoot: [String]
    var recommendedLayout: WindowsLayout?

    enum CodingKeys: String, CodingKey {
        case version, arch, editions, build, server, languages
        case installImage = "install_image"
        case installImageSize = "install_image_size"
        case imageFormat = "image_format"
        case needsSplit = "needs_split"
        case splitBlocker = "split_blocker"
        case biosBoot = "bios_boot"
        case uefiBoot = "uefi_boot"
        case recommendedLayout = "recommended_layout"
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        version = c.value(.version, "Windows")
        arch = c.value(.arch, "")
        installImage = c.value(.installImage, "")
        installImageSize = c.value(.installImageSize, 0)
        editions = c.value(.editions, [])
        build = c.optional(.build)
        server = c.value(.server, false)
        languages = c.value(.languages, [])
        imageFormat = c.value(.imageFormat, "")
        needsSplit = c.value(.needsSplit, false)
        splitBlocker = c.optional(.splitBlocker)
        biosBoot = c.value(.biosBoot, false)
        uefiBoot = c.value(.uefiBoot, [])
        recommendedLayout = c.optional(.recommendedLayout)
    }
}

struct LinuxInfo: Decodable, Sendable, Hashable {
    var distro: String
    var hasEfi: Bool
    var hasIsolinux: Bool
    var hasGrub: Bool
    var persistence: String?

    enum CodingKeys: String, CodingKey {
        case distro, persistence
        case hasEfi = "has_efi"
        case hasIsolinux = "has_isolinux"
        case hasGrub = "has_grub"
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        distro = c.value(.distro, "Linux")
        hasEfi = c.value(.hasEfi, false)
        hasIsolinux = c.value(.hasIsolinux, false)
        hasGrub = c.value(.hasGrub, false)
        persistence = c.optional(.persistence)
    }
}

struct RpiInfo: Decodable, Sendable, Hashable {
    var os: String
    var method: String

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        os = c.value(.os, "Raspberry Pi OS")
        method = c.value(.method, "")
    }

    enum CodingKeys: String, CodingKey { case os, method }
}

struct ChecksumFileInfo: Decodable, Sendable, Hashable {
    var path: String
    var algorithm: String
    var expected: String
    /// Only in hash results.
    var matches: Bool?
}

struct ImageInspection: Decodable, Sendable, Hashable {
    var path: String
    var fileName: String
    var format: String
    var container: JSONValue?
    var fileSize: UInt64
    var compressed: Bool
    var size: UInt64?
    var sizeEstimate: UInt64?
    var randomAccess: Bool
    var partitionScheme: String?
    var partitions: [ImagePartition]
    var iso: IsoInfo?
    var windows: WindowsInfo?
    var linux: LinuxInfo?
    var rpi: RpiInfo?
    var checksumFile: ChecksumFileInfo?
    var recommendedMode: ForgeWriteMode
    var explanation: String
    var notes: [String]

    enum CodingKeys: String, CodingKey {
        case path, format, container, compressed, size, partitions, iso, windows, linux, rpi, explanation, notes
        case fileName = "file_name"
        case fileSize = "file_size"
        case sizeEstimate = "size_estimate"
        case randomAccess = "random_access"
        case partitionScheme = "partition_scheme"
        case checksumFile = "checksum_file"
        case recommendedMode = "recommended_mode"
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        path = try c.decode(String.self, forKey: .path)
        fileName = c.value(.fileName, (path as NSString).lastPathComponent)
        format = c.value(.format, "raw")
        container = c.optional(.container)
        fileSize = c.value(.fileSize, 0)
        compressed = c.value(.compressed, false)
        size = c.optional(.size)
        sizeEstimate = c.optional(.sizeEstimate)
        randomAccess = c.value(.randomAccess, true)
        partitionScheme = c.optional(.partitionScheme)
        partitions = c.value(.partitions, [])
        iso = c.optional(.iso)
        windows = c.optional(.windows)
        linux = c.optional(.linux)
        rpi = c.optional(.rpi)
        checksumFile = c.optional(.checksumFile)
        recommendedMode = c.value(.recommendedMode, .dd)
        explanation = c.value(.explanation, "")
        notes = c.value(.notes, [])
    }

    /// The disk image's size (decompressed), or the best estimate.
    var imageSize: UInt64? { size ?? sizeEstimate }
}

// MARK: - Jobs (job.rs Snapshot)

enum ForgeJobState: String, Codable, Sendable {
    case running, succeeded, failed, cancelled
}

enum ForgePhase: String, Codable, Sendable, CaseIterable, Identifiable {
    case preparing, hashing, unmounting, wiping, partitioning, formatting, writing, extracting, verifying, customizing, finishing
    case unknown

    var id: String { rawValue }

    init(from decoder: any Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = ForgePhase(rawValue: raw) ?? .unknown
    }
}

enum ForgeLogLevel: String, Codable, Sendable {
    case info, warn, error
}

struct ForgeLogLine: Decodable, Sendable, Hashable, Identifiable {
    var seq: UInt64
    var ms: UInt64
    var level: ForgeLogLevel
    var msg: String

    var id: UInt64 { seq }

    init(seq: UInt64, ms: UInt64, level: ForgeLogLevel, msg: String) {
        self.seq = seq; self.ms = ms; self.level = level; self.msg = msg
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        seq = try c.decode(UInt64.self, forKey: .seq)
        ms = c.value(.ms, 0)
        level = c.value(.level, .info)
        msg = c.value(.msg, "")
    }

    enum CodingKeys: String, CodingKey { case seq, ms, level, msg }
}

struct ForgeSnapshot: Decodable, Sendable, Equatable {
    var id: UInt64
    var kind: String
    var state: ForgeJobState
    var phase: ForgePhase
    var phases: [ForgePhase]
    var done: UInt64
    var total: UInt64
    var unit: String
    var rate: Double
    var etaSecs: UInt64?
    var elapsedSecs: Double
    var logs: [ForgeLogLine]
    var result: JSONValue?
    var error: String?

    enum CodingKeys: String, CodingKey {
        case id, kind, state, phase, phases, done, total, unit, rate, logs, result, error
        case etaSecs = "eta_secs"
        case elapsedSecs = "elapsed_secs"
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(UInt64.self, forKey: .id)
        kind = c.value(.kind, "")
        state = try c.decode(ForgeJobState.self, forKey: .state)
        phase = c.value(.phase, .preparing)
        phases = c.value(.phases, [])
        done = c.value(.done, 0)
        total = c.value(.total, 0)
        unit = c.value(.unit, "bytes")
        rate = c.value(.rate, 0)
        etaSecs = c.optional(.etaSecs)
        elapsedSecs = c.value(.elapsedSecs, 0)
        logs = c.value(.logs, [])
        let r: JSONValue? = c.optional(.result)
        result = r == .null ? nil : r
        error = c.optional(.error)
    }

    var isRunning: Bool { state == .running }

    /// 0…1, or nil when the current phase has no known total (indeterminate).
    var fraction: Double? { ForgeFormat.fraction(done: done, total: total) }
}

struct ForgeStartReply: Decodable, Sendable {
    var job: UInt64
}

// MARK: - Partition doctor (doctor.rs report)

enum DoctorSeverity: String, Codable, Sendable, Comparable {
    case info, warning, error

    private var rank: Int {
        switch self {
        case .info: 0
        case .warning: 1
        case .error: 2
        }
    }

    static func < (a: DoctorSeverity, b: DoctorSeverity) -> Bool { a.rank < b.rank }
}

struct DoctorFinding: Decodable, Sendable, Hashable, Identifiable {
    var code: String
    var severity: DoctorSeverity
    var message: String
    var repairable: Bool

    var id: String { code + message }
}

struct DoctorPartition: Decodable, Sendable, Hashable {
    var index: UInt32
    var start: UInt64
    var size: UInt64
    var type: String
    var name: String

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        index = c.value(.index, 0)
        start = c.value(.start, 0)
        size = c.value(.size, 0)
        type = c.value(.type, "")
        name = c.value(.name, "")
    }

    enum CodingKeys: String, CodingKey { case index, start, size, type, name }
}

struct DoctorReport: Decodable, Sendable, Hashable {
    var scheme: String
    var sectorSize: UInt64
    var diskSize: UInt64
    var partitions: [DoctorPartition]
    var findings: [DoctorFinding]
    var healthy: Bool
    var repairable: Bool

    enum CodingKeys: String, CodingKey {
        case scheme, partitions, findings, healthy, repairable
        case sectorSize = "sector_size"
        case diskSize = "disk_size"
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        scheme = c.value(.scheme, "none")
        sectorSize = c.value(.sectorSize, 512)
        diskSize = c.value(.diskSize, 0)
        partitions = c.value(.partitions, [])
        findings = c.value(.findings, [])
        healthy = c.value(.healthy, false)
        repairable = c.value(.repairable, false)
    }

    /// A check returns the report itself; repair / convert return {before, after, actions, warnings}.
    static func from(result: JSONValue?) -> DoctorReport? {
        guard let result else { return nil }
        if let after = result["after"], let report = after.decoded(as: DoctorReport.self) { return report }
        return result.decoded(as: DoctorReport.self)
    }
}
