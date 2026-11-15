import DiskArbitration
import Foundation
import Observation
import os

// MARK: - Sendable snapshots of DiskArbitration descriptions

/// Immutable snapshot of a `DADisk` description, safe to send across isolation domains.
struct DiskDescription: Sendable, Hashable, Identifiable {
    var bsdName: String
    var isWhole: Bool
    var isLeaf: Bool
    var mediaName: String
    var mediaSize: UInt64
    var mediaContent: String
    var mediaUUID: String?
    var volumeKind: String?
    var volumeName: String?
    var volumeUUID: String?
    var volumePath: URL?
    var deviceProtocol: String?
    var deviceModel: String?
    var deviceVendor: String?
    var isInternal: Bool?
    var isRemovable: Bool
    var isEjectable: Bool
    var isWritable: Bool
    /// Filesystem found by LumenDisk's own read-only probe when macOS recognised nothing
    /// (ext4 superfloppies, raw images attached with hdiutil). Nil when not probed.
    var probedFilesystem: FilesystemKind? = nil
    /// Volume label read by the same probe ("CHIAVETTA").
    var probedLabel: String? = nil

    var id: String { bsdName }
    var wholeDiskName: String { DeviceNode.wholeDisk(of: bsdName) }
    var devicePath: String { "/dev/" + bsdName }
    var isMountedByMacOS: Bool { volumePath != nil }
    var isDiskImage: Bool { deviceModel == "Disk Image" }

    /// APFS synthesized container disks and their volumes are macOS internals, never ours.
    var isAPFSSynthesized: Bool { isWhole && mediaContent == DiskContent.apfsSynthesized }

    var isExternalCandidate: Bool {
        if isAPFSSynthesized { return false }
        if isInternal == true && !isRemovable { return false }
        return true
    }

    /// Best guess of the filesystem from what macOS knows (DAVolumeKind or partition content hint).
    var filesystemHint: FilesystemKind {
        switch volumeKind?.lowercased() {
        case "msdos":
            if mediaContent.contains("FAT_12") { return .fat12 }
            if mediaContent.contains("FAT_16") { return .fat16 }
            return .fat32
        case "exfat": return .exfat
        case "ntfs": return .ntfs
        case "apfs": return .apfs
        case "hfs": return .hfs
        default: break
        }
        if let probedFilesystem, probedFilesystem != .unknown { return probedFilesystem }
        return DiskContent.filesystemHint(for: mediaContent)
    }

    /// Something LumenDisk can very likely mount (known filesystem, Linux/Windows partition type
    /// or a successful probe).
    var isLumenDiskCandidate: Bool {
        filesystemHint.isSupportedByCore || DiskContent.isLikelySupported(mediaContent)
    }

    /// macOS left it unmounted although it holds a filesystem LumenDisk reads.
    var isReadyForLumenDisk: Bool { !isMountedByMacOS && isLumenDiskCandidate }

    /// macOS found nothing it knows on this leaf, so it is worth a read-only probe.
    var needsProbe: Bool {
        !isMountedByMacOS && volumeKind == nil && (isLeaf || isWhole)
            && DiskContent.filesystemHint(for: mediaContent) == .unknown
            && !isHiddenPartitionType && !isAPFSSynthesized
    }

    var isHiddenPartitionType: Bool { DiskContent.hidden.contains(mediaContent.uppercased()) }

    var displayName: String {
        if let volumeName, !volumeName.isEmpty { return volumeName }
        if let probedLabel, !probedLabel.isEmpty { return probedLabel }
        if !mediaName.isEmpty, !isWhole { return mediaName }
        return bsdName
    }
}

enum DiskContent {
    static let apfsSynthesized = "EF57347C-0000-11AA-AA11-00306543ECAC"
    static let apfsContainer = "7C3457EF-0000-11AA-AA11-00306543ECAC"
    static let efi = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
    static let linuxData = "0FC63DAF-8483-4772-8E79-3D69D8477DE4"
    static let microsoftBasicData = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
    static let linuxSwap = "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F"

    /// Partition types that make no sense to show (boot/EFI/APFS plumbing).
    static let hidden: Set<String> = [efi, apfsContainer, "EFI", "APPLE_BOOT", "APPLE_APFS", "APPLE_APFS_ISC", "APPLE_APFS_RECOVERY", linuxSwap, "LINUX_SWAP"]

    static func filesystemHint(for content: String) -> FilesystemKind {
        let c = content.uppercased()
        if c == linuxData || c == "LINUX" || c.contains("LINUX_FILESYSTEM") || c == "0X83" { return .linux }
        if c.contains("NTFS") || c == "WINDOWS_NTFS" { return .ntfs }
        if c.contains("FAT_32") || c == "DOS_FAT_32" || c == "WINDOWS_FAT_32" { return .fat32 }
        if c.contains("FAT_16") { return .fat16 }
        if c.contains("FAT_12") { return .fat12 }
        if c.contains("EXFAT") { return .exfat }
        if c == microsoftBasicData { return .unknown }   // NTFS, exFAT or FAT: needs inspection
        if c.contains("APFS") || c == apfsContainer { return .apfs }
        if c.contains("HFS") { return .hfs }
        return .unknown
    }

    /// Partition content that is very likely readable by the core even without a DAVolumeKind.
    static func isLikelySupported(_ content: String) -> Bool {
        let hint = filesystemHint(for: content)
        return hint.isSupportedByCore || content.uppercased() == microsoftBasicData
    }
}

/// Result of LumenDisk's read-only probe of a device macOS did not recognise.
struct ProbeOutcome: Sendable, Hashable {
    var filesystem: FilesystemKind
    var label: String?
}

/// A whole external disk and the partitions LumenDisk may care about.
struct PhysicalDisk: Sendable, Hashable, Identifiable {
    var whole: DiskDescription
    /// Leaf partitions; for a superfloppy (no partition table) this contains the whole disk itself.
    var partitions: [DiskDescription]

    var id: String { whole.bsdName }

    var title: String {
        let vendor = whole.deviceVendor?.trimmingCharacters(in: .whitespaces) ?? ""
        let model = whole.deviceModel?.trimmingCharacters(in: .whitespaces) ?? ""
        let joined = [vendor, model].filter { !$0.isEmpty && $0 != "Apple" }.joined(separator: " ")
        if whole.isDiskImage { return L("Immagine disco collegata (\(whole.bsdName))", "Attached disk image (\(whole.bsdName))") }
        if !joined.isEmpty { return joined }
        return whole.mediaName.isEmpty ? whole.bsdName : whole.mediaName
    }

    var symbol: String {
        let proto = whole.deviceProtocol?.lowercased() ?? ""
        if proto.contains("secure digital") || proto.contains("sd") && !proto.contains("usb") { return "sdcard.fill" }
        if whole.isDiskImage { return "cylinder.split.1x2.fill" }
        return "externaldrive.fill"
    }
}

enum DiskEvent: Sendable {
    case appeared(DiskDescription)
    case changed(DiskDescription)
    case disappeared(String)
}

struct DiskArbitrationError: LocalizedError, Sendable {
    var status: Int32
    var message: String?
    var errorDescription: String? {
        if let message, !message.isEmpty { return message }
        switch UInt32(bitPattern: status) {
        case UInt32(bitPattern: Int32(kDAReturnBusy)): return L("Il volume è in uso da un'altra app.", "The volume is in use by another app.")
        case UInt32(bitPattern: Int32(kDAReturnNotPermitted)), UInt32(bitPattern: Int32(kDAReturnNotPrivileged)):
            return L("macOS non ha concesso l'operazione sul disco.", "macOS did not allow the disk operation.")
        default:
            let code = String(format: "0x%08X", UInt32(bitPattern: status))
            return L("Operazione sul disco non riuscita (codice \(code)).", "The disk operation failed (code \(code)).")
        }
    }
}

// MARK: - DiskArbitration engine (runs on its own serial queue)

/// Owns the DASession. All DiskArbitration calls happen on `queue`; mutable state is either
/// confined to that queue or protected by a lock, hence `@unchecked Sendable`.
final class DiskArbitrationEngine: @unchecked Sendable {
    private let queue = DispatchQueue(label: "com.lumendisk.diskarbitration", qos: .utility)
    private var session: DASession?                 // queue-confined
    private var continuation: AsyncStream<DiskEvent>.Continuation?   // queue-confined
    private let managedIdentifiers = OSAllocatedUnfairLock<Set<String>>(initialState: [])

    /// Starts the session and returns the stream of hot-plug events (initial disks included).
    func events() -> AsyncStream<DiskEvent> {
        let (stream, continuation) = AsyncStream<DiskEvent>.makeStream(bufferingPolicy: .unbounded)
        queue.async { [self] in
            self.continuation = continuation
            guard session == nil, let session = DASessionCreate(kCFAllocatorDefault) else { return }
            self.session = session
            let context = Unmanaged.passUnretained(self).toOpaque()

            DARegisterDiskAppearedCallback(session, nil, { disk, context in
                guard let context else { return }
                let engine = Unmanaged<DiskArbitrationEngine>.fromOpaque(context).takeUnretainedValue()
                if let description = DiskArbitrationEngine.describe(disk) { engine.continuation?.yield(.appeared(description)) }
            }, context)

            DARegisterDiskDisappearedCallback(session, nil, { disk, context in
                guard let context, let bsd = DADiskGetBSDName(disk) else { return }
                let engine = Unmanaged<DiskArbitrationEngine>.fromOpaque(context).takeUnretainedValue()
                engine.continuation?.yield(.disappeared(String(cString: bsd)))
            }, context)

            DARegisterDiskDescriptionChangedCallback(session, nil, nil, { disk, _, context in
                guard let context else { return }
                let engine = Unmanaged<DiskArbitrationEngine>.fromOpaque(context).takeUnretainedValue()
                if let description = DiskArbitrationEngine.describe(disk) { engine.continuation?.yield(.changed(description)) }
            }, context)

            // Keep macOS from natively mounting volumes that an automount rule hands to LumenDisk.
            DARegisterDiskMountApprovalCallback(session, nil, { disk, context in
                guard let context else { return nil }
                let engine = Unmanaged<DiskArbitrationEngine>.fromOpaque(context).takeUnretainedValue()
                guard let description = DiskArbitrationEngine.describe(disk) else { return nil }
                let ids = engine.managedIdentifiers.withLock { $0 }
                let keys = [description.volumeUUID, description.mediaUUID].compactMap { $0?.uppercased() }
                guard keys.contains(where: ids.contains) else { return nil }
                let dissenter = DADissenterCreate(kCFAllocatorDefault, DAReturn(kDAReturnNotPermitted), "Gestito da LumenDisk" as CFString)
                return Unmanaged.passRetained(dissenter)
            }, context)

            DASessionSetDispatchQueue(session, queue)
        }
        continuation.onTermination = { [weak self] _ in self?.stop() }
        return stream
    }

    func stop() {
        queue.async { [self] in
            if let session { DASessionSetDispatchQueue(session, nil) }
            session = nil
            continuation?.finish()
            continuation = nil
        }
    }

    /// Volume/media UUIDs (uppercased) that macOS must not mount natively.
    func setManagedIdentifiers(_ ids: Set<String>) {
        managedIdentifiers.withLock { $0 = Set(ids.map { $0.uppercased() }) }
    }

    /// Unmounts a volume (or, with `whole`, every volume of the disk) and waits for DA's callback.
    func unmount(bsdName: String, whole: Bool = false, force: Bool = false) async throws {
        var options = DADiskUnmountOptions(kDADiskUnmountOptionDefault)
        if whole { options |= DADiskUnmountOptions(kDADiskUnmountOptionWhole) }
        if force { options |= DADiskUnmountOptions(kDADiskUnmountOptionForce) }
        let finalOptions = options
        try await perform(bsdName: bsdName) { disk, box in
            DADiskUnmount(disk, finalOptions, { _, dissenter, context in
                DiskArbitrationEngine.complete(context, dissenter)
            }, box)
        }
    }

    /// Ejects the whole device (after its volumes have been unmounted).
    func eject(bsdName: String) async throws {
        let wholeName = DeviceNode.wholeDisk(of: bsdName)
        try await perform(bsdName: wholeName) { disk, box in
            DADiskEject(disk, DADiskEjectOptions(kDADiskEjectOptionDefault), { _, dissenter, context in
                DiskArbitrationEngine.complete(context, dissenter)
            }, box)
        }
    }

    // MARK: Private

    private final class ContinuationBox {
        let continuation: CheckedContinuation<Void, any Error>
        init(_ continuation: CheckedContinuation<Void, any Error>) { self.continuation = continuation }
    }

    private func perform(
        bsdName: String,
        _ operation: @escaping @Sendable (DADisk, UnsafeMutableRawPointer) -> Void
    ) async throws {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, any Error>) in
            queue.async { [self] in
                guard let session,
                      let disk = DADiskCreateFromBSDName(kCFAllocatorDefault, session, bsdName) else {
                    continuation.resume(throwing: DiskArbitrationError(status: Int32(bitPattern: UInt32(kDAReturnNotFound)), message: L("Disco \(bsdName) non trovato.", "Disk \(bsdName) not found.")))
                    return
                }
                let box = Unmanaged.passRetained(ContinuationBox(continuation)).toOpaque()
                operation(disk, box)
            }
        }
    }

    private static func complete(_ context: UnsafeMutableRawPointer?, _ dissenter: DADissenter?) {
        guard let context else { return }
        let box = Unmanaged<ContinuationBox>.fromOpaque(context).takeRetainedValue()
        if let dissenter {
            let status = DADissenterGetStatus(dissenter)
            let message = DADissenterGetStatusString(dissenter) as String?
            box.continuation.resume(throwing: DiskArbitrationError(status: status, message: message))
        } else {
            box.continuation.resume()
        }
    }

    static func describe(_ disk: DADisk) -> DiskDescription? {
        guard let raw = DADiskCopyDescription(disk) as NSDictionary? else { return nil }
        func value(_ key: CFString) -> Any? { raw[key as String] }
        func string(_ key: CFString) -> String? { value(key) as? String }
        func bool(_ key: CFString) -> Bool? { (value(key) as? NSNumber)?.boolValue }
        func uuid(_ key: CFString) -> String? {
            guard let v = value(key) else { return nil }
            let ref = v as CFTypeRef
            guard CFGetTypeID(ref) == CFUUIDGetTypeID() else { return nil }
            let cfuuid = unsafeDowncast(ref, to: CFUUID.self)
            return CFUUIDCreateString(kCFAllocatorDefault, cfuuid) as String?
        }

        guard let bsd = string(kDADiskDescriptionMediaBSDNameKey), !bsd.isEmpty else { return nil }
        return DiskDescription(
            bsdName: bsd,
            isWhole: bool(kDADiskDescriptionMediaWholeKey) ?? false,
            isLeaf: bool(kDADiskDescriptionMediaLeafKey) ?? false,
            mediaName: string(kDADiskDescriptionMediaNameKey) ?? "",
            mediaSize: (value(kDADiskDescriptionMediaSizeKey) as? NSNumber)?.uint64Value ?? 0,
            mediaContent: string(kDADiskDescriptionMediaContentKey) ?? "",
            mediaUUID: uuid(kDADiskDescriptionMediaUUIDKey),
            volumeKind: string(kDADiskDescriptionVolumeKindKey),
            volumeName: string(kDADiskDescriptionVolumeNameKey),
            volumeUUID: uuid(kDADiskDescriptionVolumeUUIDKey),
            volumePath: value(kDADiskDescriptionVolumePathKey) as? URL,
            deviceProtocol: string(kDADiskDescriptionDeviceProtocolKey),
            deviceModel: string(kDADiskDescriptionDeviceModelKey),
            deviceVendor: string(kDADiskDescriptionDeviceVendorKey),
            isInternal: bool(kDADiskDescriptionDeviceInternalKey),
            isRemovable: bool(kDADiskDescriptionMediaRemovableKey) ?? false,
            isEjectable: bool(kDADiskDescriptionMediaEjectableKey) ?? false,
            isWritable: bool(kDADiskDescriptionMediaWritableKey) ?? false
        )
    }
}

// MARK: - Main-actor model

/// Observable list of external/removable disks, fed by DiskArbitration hot-plug callbacks.
@MainActor
@Observable
final class DiskMonitor {
    private(set) var disks: [PhysicalDisk] = []

    /// Called once per partition the first time it appears (and again after re-plug).
    @ObservationIgnored var onPartitionAppeared: ((DiskDescription) -> Void)?
    /// Called after a rebuild that added or removed partitions, or changed one's identity
    /// (media UUID, label), after the `onPartitionAppeared` calls.
    @ObservationIgnored var onPartitionsChanged: (() -> Void)?

    @ObservationIgnored private var descriptions: [String: DiskDescription] = [:]
    @ObservationIgnored private let engine = DiskArbitrationEngine()
    @ObservationIgnored private var listenTask: Task<Void, Never>?
    @ObservationIgnored private var rebuildScheduled = false
    @ObservationIgnored private var announced: Set<String> = []
    /// Read-only probe of a device node macOS could not identify (set by AppModel, backed by
    /// ld_inspect_source). Only used on nodes this user can already read, so it never triggers
    /// an authorisation prompt.
    @ObservationIgnored var prober: (@MainActor (String) async -> ProbeOutcome?)?
    @ObservationIgnored private var probed: [String: ProbeOutcome] = [:]
    @ObservationIgnored private var probing: Set<String> = []

    func start() {
        guard listenTask == nil else { return }
        let stream = engine.events()
        listenTask = Task { [weak self] in
            for await event in stream {
                guard let self else { return }
                self.handle(event)
            }
        }
    }

    func stop() {
        listenTask?.cancel()
        listenTask = nil
        engine.stop()
    }

    func setManagedIdentifiers(_ ids: Set<String>) { engine.setManagedIdentifiers(ids) }

    func partition(bsdName: String) -> DiskDescription? {
        disks.lazy.flatMap(\.partitions).first { $0.bsdName == bsdName }
    }

    func disk(containing bsdName: String) -> PhysicalDisk? {
        disks.first { $0.id == DeviceNode.wholeDisk(of: bsdName) }
    }

    func unmountNative(bsdName: String) async throws {
        try await engine.unmount(bsdName: bsdName)
    }

    func unmountWholeNative(bsdName: String) async throws {
        try await engine.unmount(bsdName: DeviceNode.wholeDisk(of: bsdName), whole: true)
    }

    func eject(bsdName: String) async throws {
        try await engine.eject(bsdName: bsdName)
    }

    // MARK: Event handling

    private func handle(_ event: DiskEvent) {
        switch event {
        case .appeared(let d), .changed(let d):
            descriptions[d.bsdName] = d
        case .disappeared(let bsd):
            descriptions[bsd] = nil
            announced.remove(bsd)
            probed[bsd] = nil
        }
        scheduleRebuild()
    }

    /// Coalesces bursts of callbacks (a disk with 4 partitions fires ~10 events).
    private func scheduleRebuild() {
        guard !rebuildScheduled else { return }
        rebuildScheduled = true
        Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(250))
            guard let self else { return }
            self.rebuildScheduled = false
            self.rebuild()
        }
    }

    /// Forget the probe of a node (e.g. after LumenDisk unmounted or reformatted it).
    func reprobe(bsdName: String) {
        probed[bsdName] = nil
        scheduleRebuild()
    }

    private func probeIfNeeded(_ d: DiskDescription) {
        guard let prober, d.needsProbe, probed[d.bsdName] == nil, !probing.contains(d.bsdName),
              FileManager.default.isReadableFile(atPath: d.devicePath) else { return }
        probing.insert(d.bsdName)
        Task { [weak self] in
            let kind = await prober(d.devicePath)
            guard let self else { return }
            self.probing.remove(d.bsdName)
            guard self.descriptions[d.bsdName] != nil else { return }
            self.probed[d.bsdName] = kind ?? ProbeOutcome(filesystem: .unknown, label: nil)
            self.scheduleRebuild()
        }
    }

    private func rebuild() {
        var result: [PhysicalDisk] = []
        let all = descriptions.values.map { description -> DiskDescription in
            var d = description
            if d.needsProbe {
                if let outcome = probed[d.bsdName] {
                    d.probedFilesystem = outcome.filesystem
                    d.probedLabel = outcome.label
                } else {
                    probeIfNeeded(d)
                }
            }
            return d
        }
        let wholes = all.filter { $0.isWhole && $0.isExternalCandidate }
        for whole in wholes {
            let children = all
                .filter { !$0.isWhole && $0.wholeDiskName == whole.bsdName }
                .filter { $0.isLeaf || $0.volumeKind != nil }
                .filter { !$0.isHiddenPartitionType }
                .sorted { $0.bsdName.localizedStandardCompare($1.bsdName) == .orderedAscending }

            var partitions = children
            if partitions.isEmpty, whole.volumeKind != nil || whole.isLeaf {
                partitions = [whole]   // superfloppy: filesystem directly on the whole disk
            }
            // Disk images attached by other apps (simulators, installers) only matter when they
            // contain something LumenDisk can mount.
            if whole.isDiskImage {
                partitions = partitions.filter(\.isLumenDiskCandidate)
                if partitions.isEmpty { continue }
            }
            // Hide APFS-only disks entirely only if they are virtual.
            if partitions.isEmpty && whole.deviceProtocol == "Virtual Interface" { continue }
            result.append(PhysicalDisk(whole: whole, partitions: partitions))
        }
        result.sort { $0.whole.bsdName.localizedStandardCompare($1.whole.bsdName) == .orderedAscending }
        let identity = { (list: [PhysicalDisk]) in
            Set(list.flatMap(\.partitions).map { "\($0.bsdName)|\($0.mediaUUID ?? "")|\($0.volumeName ?? "")|\($0.probedLabel ?? "")" })
        }
        let partitionsChanged = identity(result) != identity(disks)
        if result != disks { disks = result }

        for partition in result.flatMap(\.partitions) where !announced.contains(partition.bsdName) {
            announced.insert(partition.bsdName)
            onPartitionAppeared?(partition)
        }
        if partitionsChanged { onPartitionsChanged?() }
    }
}
