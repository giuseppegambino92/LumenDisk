import AppKit
import Foundation
import Observation

enum SidebarItem: Hashable, Sendable {
    case volume(String)
    case physical(String)
    case network
    case lanPeer(String)
    case shadow
    case ghost
    case images
    case rules
}

struct AppAlert: Identifiable, Sendable {
    let id = UUID()
    var title: String
    var message: String
}

/// Sheets presented by the main window (one at a time).
enum AppSheet: Identifiable, Hashable, Sendable {
    case connectServer
    case imageURL
    case pair(LANPeer)
    case portal(String)   // volume id

    var id: String {
        switch self {
        case .connectServer: "connect"
        case .imageURL: "imageurl"
        case .pair(let peer): "pair:" + peer.id
        case .portal(let id): "portal:" + id
        }
    }
}

struct LockInspection: Identifiable, Sendable {
    var volumeID: String
    var mountpoint: String
    var volumeName: String
    /// Message of the failed unmount that triggered the inspector, if any.
    var failureMessage: String?
    var id: String { volumeID }
}

/// Last speed test of a volume during this session.
struct SpeedTestRecord: Sendable, Equatable {
    var result: BenchmarkResult?
    var date: Date?
    var isRunning = false
    var startedAt: Date?
    var error: String?
}

/// Hardware report of a physical disk, loaded on demand.
enum DiskHealthLoad: Sendable, Equatable {
    case loading
    case loaded(DiskHealth)
    case failed(String)
}

extension VolumeStatus {
    /// Stable across remounts within a session (the volume id is not).
    var speedTestKey: String {
        isRemote ? "remote:" + remoteURL : "local:\(deviceBSDName.map { "/dev/" + $0 } ?? sourcePath)#\(partition)"
    }
}

/// Options used when mounting a source through LumenDisk.
struct MountOptions: Sendable, Hashable {
    var mode: MountMode = .readWrite
    var ghostShield: Bool = true
    var uidMap: UIDMapRequest = .automatic

    init(mode: MountMode = .readWrite, ghostShield: Bool = true, uidMap: UIDMapRequest = .automatic) {
        self.mode = mode
        self.ghostShield = ghostShield
        self.uidMap = uidMap
    }

    init(rule: AutomountRule) {
        self.init(mode: rule.mode, ghostShield: rule.ghostShield, uidMap: rule.uidMapRequest)
    }
}

/// State of the disk-image workflow (inspection → partition choice → mount).
struct ImageSession: Sendable {
    struct PartitionChoice: Sendable, Hashable {
        var selected: Bool
        var mode: MountMode
        var ruleName: String?
    }

    var url: URL
    var inspection: SourceInspection?
    var isInspecting = true
    var isMounting = false
    var error: String?
    var choices: [UInt32: PartitionChoice] = [:]
    var ghostShield = true
    var uidMapEnabled = true
    var repairing: Set<UInt32> = []

    var selectedCount: Int { choices.values.filter(\.selected).count }
}

/// Root view model shared by the main window and the menu bar popover.
@MainActor
@Observable
final class AppModel {
    static let shared = AppModel()

    let diskMonitor = DiskMonitor()
    let rules = AutomountStore()
    let lanDiscovery = LANDiscovery()
    let settings = AppSettings.shared
    @ObservationIgnored private let bridge = CoreBridge.shared
    @ObservationIgnored private let bonjour = BonjourPublisher()
    @ObservationIgnored private lazy var desktopAliases = DesktopAliasManager()

    // Core state
    private(set) var volumes: [VolumeStatus] = []
    private(set) var isReady = false
    private(set) var startupError: String?
    private(set) var coreVersion = "—"
    private(set) var isEjectingAll = false

    // UI state
    var selection: SidebarItem?
    var alert: AppAlert?
    var lockInspection: LockInspection?
    var showCreateWizard = false
    var imageSession: ImageSession?
    var activeSheet: AppSheet?
    /// Last ShareStatus / PortalStatus returned by start calls, used when an older core does not
    /// report them in VolumeStatus.
    private(set) var startedShares: [String: ShareStatus] = [:]
    private(set) var startedPortals: [String: PortalStatus] = [:]
    var shadowFocusID: String?
    var ghostFocusID: String?
    var popoverVisible = false
    /// Set by any scene that has an `openWindow` action, so alerts raised from the menu bar can
    /// bring the main window forward even when it was closed.
    @ObservationIgnored var openMainWindowAction: (@MainActor () -> Void)?
    /// Activity labels keyed by volume id or BSD name ("Montaggio…", "Espulsione…").
    private(set) var activity: [String: String] = [:]
    private(set) var recentImages: [String] = Preferences.shared.strings(forKey: "recentImages")
    /// Speed test results, remembered for the session (keyed by `VolumeStatus.speedTestKey`).
    private(set) var speedTests: [String: SpeedTestRecord] = [:]
    /// Hardware reports keyed by BSD name.
    private(set) var diskHealthReports: [String: DiskHealthLoad] = [:]
    /// Saved Shadow sessions (mounted or not), refreshed on launch, disk hot-plug and after
    /// mount / eject / commit / rollback / discard — not on every poll tick (it may read disks).
    private(set) var shadowSessions: [ShadowSession] = []

    @ObservationIgnored private var pollTask: Task<Void, Never>?
    @ObservationIgnored private var sessionsRefreshTask: Task<Void, Never>?
    /// Recovery prompts already shown in this run: session key → `promptSignature`.
    @ObservationIgnored private var promptedSessions: [String: String] = [:]
    @ObservationIgnored private var isPromptingSessions = false
    @ObservationIgnored private var isRefreshing = false
    @ObservationIgnored private var lastRefresh = Date.distantPast
    @ObservationIgnored private var autoMountAttempted: Set<String> = []
    @ObservationIgnored var isTerminating = false

    // MARK: Derived

    var totalGhostBlocked: UInt64 { volumes.reduce(0) { $0 + ($1.ghost.enabled ? $1.ghost.blockedCount : 0) } }
    var shadowVolumes: [VolumeStatus] { volumes.filter { $0.mode == .shadow } }
    var totalShadowChanges: UInt64 { shadowVolumes.reduce(0) { $0 + ($1.shadow?.totalChanges ?? 0) } }
    var pendingShadowVolumes: [VolumeStatus] { volumes.filter(\.hasPendingShadowChanges) }
    /// Saved Shadow sessions that are not mounted right now.
    var savedShadowSessions: [ShadowSession] {
        shadowSessions.filter { session in session.mountedVolumeID.map { volume(id: $0) == nil } ?? true }
    }
    /// Pending Shadow changes on mounted volumes plus those saved for disks not mounted.
    var totalPendingShadowChanges: UInt64 {
        savedShadowSessions.reduce(totalShadowChanges) { $0 + $1.changeCount }
    }
    var localVolumes: [VolumeStatus] { volumes.filter { !$0.isRemote } }
    var remoteVolumes: [VolumeStatus] { volumes.filter(\.isRemote) }
    /// Remote volumes with changes still waiting to be uploaded.
    var pendingSyncVolumes: [VolumeStatus] { volumes.filter { $0.hasSyncQueue && ($0.remote?.pendingOps ?? 0) > 0 } }

    func share(for volume: VolumeStatus) -> ShareStatus? { volume.share ?? startedShares[volume.id] }
    func portal(for volume: VolumeStatus) -> PortalStatus? { volume.portal ?? startedPortals[volume.id] }

    /// Physical partitions that are not currently served by LumenDisk.
    var unmanagedPartitions: [DiskDescription] {
        let managed = Set(volumes.compactMap(\.deviceBSDName))
        return diskMonitor.disks.flatMap(\.partitions).filter { !managed.contains($0.bsdName) }
    }

    func volume(id: String) -> VolumeStatus? { volumes.first { $0.id == id } }
    func activity(for key: String) -> String? { activity[key] }
    func isBusy(_ key: String) -> Bool { activity[key] != nil }

    func physicalPartition(for volume: VolumeStatus) -> DiskDescription? {
        volume.deviceBSDName.flatMap { diskMonitor.partition(bsdName: $0) }
    }

    func rule(for volume: VolumeStatus) -> AutomountRule? {
        let partition = physicalPartition(for: volume)
        return rules.rule(matching: [volume.uuid, partition?.mediaUUID, partition?.volumeUUID, volume.isPhysical ? nil : volume.sourcePath])
    }

    func rule(for partition: DiskDescription) -> AutomountRule? {
        rules.rule(matching: [partition.volumeUUID, partition.mediaUUID])
    }

    // MARK: Lifecycle

    func start() async {
        guard !isReady, startupError == nil else { return }
        do {
            try await bridge.initialize(supportDirectory: AppPaths.supportDirectory)
            coreVersion = await bridge.version
            isReady = true
        } catch {
            startupError = error.localizedDescription
            return
        }

        rules.onChange = { [weak self] in self?.syncManagedIdentifiers() }
        settings.onChange = { [weak self] in self?.syncDesktopAliases() }
        #if DEBUG
        let env = ProcessInfo.processInfo.environment
        bonjour.isEnabled = env["LUMENDISK_NO_BONJOUR"] != "1"
        if env["LUMENDISK_NO_BONJOUR"] != "1" {
            lanDiscovery.start()
        }
        #else
        lanDiscovery.start()
        #endif
        syncManagedIdentifiers()
        diskMonitor.prober = { [weak self] path in
            guard let self, let inspection = try? await self.bridge.inspect(path: path) else { return nil }
            guard let partition = inspection.partitions.first(where: \.isMountable) else {
                return ProbeOutcome(filesystem: .unknown, label: nil)
            }
            return ProbeOutcome(filesystem: partition.filesystem, label: partition.label)
        }
        diskMonitor.onPartitionAppeared = { [weak self] partition in
            self?.handleAppeared(partition)
        }
        diskMonitor.onPartitionsChanged = { [weak self] in
            self?.scheduleShadowSessionsRefresh()
        }
        diskMonitor.start()

        await refresh(force: true)
        if selection == nil {
            selection = volumes.first.map { .volume($0.id) } ?? .images
        }
        startPolling()
        // Saved Shadow sessions: listed now, offered once the window and the disks plugged in at
        // launch have settled (DiskArbitration announces them over the first second or so).
        await refreshShadowSessions(prompt: false)
        scheduleShadowSessionsRefresh(after: .milliseconds(1500))
        #if DEBUG
        DebugHooks.apply(to: self)
        #endif
    }

    private func startPolling() {
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(2))
                guard let self else { return }
                // Every 2 s while some window (main window or popover) is on screen,
                // otherwise every 15 s just to keep the menu bar state fresh.
                let interval: TimeInterval = self.isAnyWindowVisible ? 1.9 : 15
                if Date().timeIntervalSince(self.lastRefresh) >= interval {
                    await self.refresh()
                }
            }
        }
    }

    private var isAnyWindowVisible: Bool {
        popoverVisible || NSApp.windows.contains { $0.isVisible && $0.occlusionState.contains(.visible) }
    }

    func refresh(force: Bool = false) async {
        guard isReady, !isRefreshing || force else { return }
        isRefreshing = true
        defer { isRefreshing = false; lastRefresh = Date() }
        do {
            let latest = try await bridge.listVolumes()
            if latest != volumes { volumes = latest }
            if case .volume(let id)? = selection, volume(id: id) == nil, activity[id] == nil {
                selection = volumes.first.map { .volume($0.id) } ?? .images
            }
            syncNetworkServices()
        } catch {
            NSLog("LumenDisk: ld_list_volumes failed: \(error.localizedDescription)")
        }
    }

    /// Bonjour advertisements and Desktop aliases follow the volume list.
    private func syncNetworkServices() {
        let ids = Set(volumes.map(\.id))
        startedShares = startedShares.filter { ids.contains($0.key) }
        startedPortals = startedPortals.filter { ids.contains($0.key) }
        var shares: [String: (VolumeStatus, ShareStatus)] = [:]
        for volume in volumes {
            // A core that reports `share` is authoritative; otherwise trust our own start call.
            if let share = volume.share ?? startedShares[volume.id] { shares[volume.id] = (volume, share) }
        }
        bonjour.reconcile(shares)
        lanDiscovery.localSharedVolumeIDs = Set(shares.keys)
        syncDesktopAliases()
    }

    private func syncDesktopAliases() {
        guard isReady, !isTerminating else { return }
        desktopAliases.sync(volumes: volumes, enabled: settings.createDesktopAlias)
    }

    private func setActivity(_ key: String, _ label: String?) {
        activity[key] = label
    }

    func bringMainWindowForward() {
        openMainWindowAction?()
        NSApp.activate()
    }

    private func present(_ title: String, _ error: any Error) {
        present(title, message: friendlyMessage(error))
    }

    private func present(_ title: String, message: String) {
        alert = AppAlert(title: title, message: message)
        bringMainWindowForward()
    }

    private func friendlyMessage(_ error: any Error) -> String {
        let base = error.localizedDescription
        let lower = base.lowercased()
        if lower.contains("authopen") || lower.contains("not authorized") || lower.contains("cancel") {
            return L("L'accesso al dispositivo non è stato autorizzato. Per montare un disco fisico serve la password di amministratore.",
                     "Access to the device was not authorised. Mounting a physical disk needs an administrator password.")
        }
        if lower.contains("permission denied") || lower.contains("operation not permitted") {
            return L("macOS ha negato l'accesso (\(base)). Verifica i permessi e riprova.",
                     "macOS denied access (\(base)). Check the permissions and try again.")
        }
        if lower.contains("not implemented") {
            return L("Questa funzione non è ancora disponibile nel motore di LumenDisk installato.",
                     "This feature is not available yet in the installed LumenDisk engine.")
        }
        return base
    }

    // MARK: Mounting

    @discardableResult
    func mount(_ request: MountRequest, activityKey: String, select: Bool = true) async -> VolumeStatus? {
        setActivity(activityKey, L("Montaggio…", "Mounting…"))
        defer { setActivity(activityKey, nil) }
        do {
            let volume = try await bridge.mount(request)
            await refresh(force: true)
            if select { selection = .volume(volume.id) }
            await refreshShadowSessions()
            return volume
        } catch {
            present(L("Impossibile montare il volume", "Could not mount the volume"), error)
            // A refused Shadow mount may have just found the disk modified elsewhere.
            await refreshShadowSessions()
            return nil
        }
    }

    /// Mount a physical partition through LumenDisk, releasing macOS's own mount first.
    /// `expectSession` resumes that saved Shadow session (the mount fails if the disk doesn't hold it).
    @discardableResult
    func mountPhysical(_ partition: DiskDescription, options: MountOptions, select: Bool = true,
                       expectSession: String? = nil) async -> VolumeStatus? {
        let key = partition.bsdName
        guard !isBusy(key) else { return nil }
        if partition.isMountedByMacOS {
            setActivity(key, L("Rilascio da macOS…", "Releasing from macOS…"))
            do {
                try await diskMonitor.unmountNative(bsdName: partition.bsdName)
            } catch {
                setActivity(key, nil)
                present(L("Impossibile liberare «\(partition.displayName)»", "Could not release “\(partition.displayName)”"),
                        message: L("macOS sta ancora usando il volume: \(error.localizedDescription) Chiudi le app che lo usano e riprova.",
                                   "macOS is still using the volume: \(error.localizedDescription) Quit the apps using it and try again."))
                return nil
            }
        }
        var request = MountRequest(path: partition.devicePath, partition: 0, mode: options.mode,
                                   ghostShield: options.ghostShield, uidMap: options.uidMap)
        request.mediaUUID = partition.mediaUUID
        request.expectSession = expectSession
        guard let volume = await mount(request, activityKey: key, select: select) else { return nil }
        rules.addAliases([partition.mediaUUID, partition.volumeUUID], toRuleWithVolumeUUID: volume.uuid, sourcePath: nil)
        return volume
    }

    private func handleAppeared(_ partition: DiskDescription) {
        guard isReady,
              let rule = rule(for: partition), rule.automount,
              !autoMountAttempted.contains(partition.bsdName),
              !volumes.contains(where: { $0.deviceBSDName == partition.bsdName }) else { return }
        var options = MountOptions(rule: rule)
        if let session = shadowSession(for: partition) {
            // The core refuses a read/write mount while Shadow changes are saved for this disk,
            // and an interrupted commit or a disk modified elsewhere needs a decision: in those
            // cases the recovery prompt (shown by the session refresh) replaces the automount.
            // A Shadow rule simply mounts it: the core restores the saved changes.
            guard rule.mode == .shadow, !session.isCommitInterrupted, !session.isStale else {
                promptedSessions[session.key] = nil   // ask even if "Later" was chosen earlier
                return
            }
            promptedSessions[session.key] = session.promptSignature
            options.mode = .shadow
        }
        autoMountAttempted.insert(partition.bsdName)
        Task {
            // Give DiskArbitration a moment to settle (macOS may still be probing the volume).
            try? await Task.sleep(for: .seconds(1))
            let current = diskMonitor.partition(bsdName: partition.bsdName) ?? partition
            guard !volumes.contains(where: { $0.deviceBSDName == partition.bsdName }) else {
                autoMountAttempted.remove(partition.bsdName)
                return
            }
            await mountPhysical(current, options: options, select: false)
            autoMountAttempted.remove(partition.bsdName)
        }
    }

    private func syncManagedIdentifiers() {
        diskMonitor.setManagedIdentifiers(rules.automountIdentifiers)
    }

    // MARK: Ejecting

    func eject(_ volume: VolumeStatus, force: Bool = false) async {
        setActivity(volume.id, volume.isRemote ? L("Disconnessione…", "Disconnecting…") : L("Espulsione…", "Ejecting…"))
        defer { setActivity(volume.id, nil) }
        desktopAliases.remove(volumeID: volume.id)
        do {
            try await bridge.unmount(volumeID: volume.id, force: force)
        } catch {
            await refresh(force: true)
            lockInspection = LockInspection(volumeID: volume.id, mountpoint: volume.mountpoint,
                                            volumeName: volume.displayName,
                                            failureMessage: error.localizedDescription)
            bringMainWindowForward()
            return
        }
        await refresh(force: true)
        await ejectDeviceIfIdle(bsdName: volume.deviceBSDName)
        await refreshShadowSessions()
    }

    /// After LumenDisk released a physical partition, eject the device unless another LumenDisk
    /// volume still lives on it.
    private func ejectDeviceIfIdle(bsdName: String?) async {
        guard let bsdName else { return }
        let whole = DeviceNode.wholeDisk(of: bsdName)
        guard !volumes.contains(where: { $0.deviceBSDName.map(DeviceNode.wholeDisk(of:)) == whole }) else { return }
        guard diskMonitor.disk(containing: bsdName) != nil else { return }
        try? await diskMonitor.unmountWholeNative(bsdName: whole)
        try? await diskMonitor.eject(bsdName: whole)
    }

    /// Process Lock Inspector → terminate selected holders, then unmount.
    func releaseLocksAndEject(_ inspection: LockInspection, pids: [Int32]) async -> Bool {
        do {
            if !pids.isEmpty {
                try await bridge.releaseLocks(mountpoint: inspection.mountpoint, pids: pids)
                try? await Task.sleep(for: .milliseconds(400))
            }
            try await bridge.unmount(volumeID: inspection.volumeID, force: false)
        } catch {
            do {
                try await bridge.unmount(volumeID: inspection.volumeID, force: true)
            } catch {
                present(L("Il disco è ancora in uso", "The disk is still in use"), error)
                return false
            }
        }
        let bsd = volume(id: inspection.volumeID)?.deviceBSDName
        await refresh(force: true)
        await ejectDeviceIfIdle(bsdName: bsd)
        await refreshShadowSessions()
        return true
    }

    func ejectAll() async {
        guard !isEjectingAll else { return }
        isEjectingAll = true
        defer { isEjectingAll = false }
        let physical = volumes.compactMap(\.deviceBSDName)
        let snapshot = volumes
        do {
            let result = try await bridge.unmountAll()
            await refresh(force: true)
            for bsd in Set(physical.map(DeviceNode.wholeDisk(of:))) { await ejectDeviceIfIdle(bsdName: bsd) }
            await refreshShadowSessions()
            let failedNames = names(ofFailed: result.failed, among: snapshot)
            if !failedNames.isEmpty {
                present(L("Alcuni volumi sono ancora in uso", "Some volumes are still in use"),
                        message: L("Non è stato possibile espellere: \(failedNames.joined(separator: ", ")). Apri la scheda del volume e usa «Chi Blocca il Disco?» per scoprire quale app lo sta usando.",
                                   "Could not eject: \(failedNames.joined(separator: ", ")). Open the volume and use “Who’s Using the Disk?” to find out which app is holding it."))
            }
        } catch {
            present(L("Espulsione non riuscita", "Eject failed"), error)
        }
    }

    /// The core reports failures as "<volume id>: <reason>"; people need the volume name.
    private func names(ofFailed failed: [String], among snapshot: [VolumeStatus]) -> [String] {
        failed.map { entry in
            let id = entry.split(separator: ":", maxSplits: 1).first.map(String.init)?.trimmingCharacters(in: .whitespaces) ?? entry
            return snapshot.first { $0.id == id || $0.id == entry }?.displayName ?? entry
        }
    }

    /// Used by applicationShouldTerminate. Returns the names that could not be unmounted.
    func ejectAllForQuit() async -> [String] {
        // Restored by the next refresh if the user cancels the quit.
        desktopAliases.removeAll()
        bonjour.stopAll()
        let snapshot = volumes
        do {
            let result = try await bridge.unmountAll()
            var failed: [String] = []
            for entry in result.failed {
                let id = entry.split(separator: ":", maxSplits: 1).first.map(String.init)?.trimmingCharacters(in: .whitespaces) ?? entry
                // Persistent Shadow changes are kept on this Mac (the quit dialog said so):
                // the core refuses a plain eject while they are pending, so force it.
                if let v = snapshot.first(where: { $0.id == id }), v.keepsShadowChanges,
                   (try? await bridge.unmount(volumeID: v.id, force: true)) != nil {
                    continue
                }
                failed.append(entry)
            }
            return names(ofFailed: failed, among: snapshot)
        } catch {
            return volumes.map(\.displayName)
        }
    }

    /// Last-chance cleanup from applicationWillTerminate: nothing of ours stays on the Desktop
    /// or on the network.
    func prepareForQuit() {
        isTerminating = true
        desktopAliases.removeAll()
        bonjour.stopAll()
        lanDiscovery.stop()
    }

    // MARK: Mode, Ghost Shield, UID bridge

    enum ShadowResolution: Sendable { case commit, rollback }

    func changeMode(_ volume: VolumeStatus, to mode: MountMode, resolving resolution: ShadowResolution? = nil) async {
        guard mode != volume.mode, !volume.isRemote else { return }
        setActivity(volume.id, L("Rimontaggio…", "Remounting…"))
        defer { setActivity(volume.id, nil) }
        do {
            switch resolution {
            case .commit?: try await bridge.shadowCommit(volumeID: volume.id)
            case .rollback?: try await bridge.shadowRollback(volumeID: volume.id)
            case nil: break
            }
        } catch {
            await refresh(force: true)   // a failed commit leaves the volume in "committing" state
            present(L("Impossibile cambiare modalità", "Could not change the mode"), error)
            return
        }

        do {
            try await bridge.unmount(volumeID: volume.id)
        } catch {
            lockInspection = LockInspection(volumeID: volume.id, mountpoint: volume.mountpoint,
                                            volumeName: volume.displayName,
                                            failureMessage: L("Per cambiare modalità il volume deve essere rimontato, ma è in uso. ",
                                                              "To change the mode the volume must be remounted, but it is in use. ") + error.localizedDescription)
            return
        }

        var request = MountRequest(
            path: volume.sourcePath, partition: volume.partition, mode: mode,
            ghostShield: volume.ghost.enabled,
            uidMap: UIDMapRequest(enabled: volume.uidMap.enabled, linuxUid: volume.uidMap.linuxUid, linuxGid: volume.uidMap.linuxGid),
            mountRoot: volume.mountpoint.isEmpty ? AppPaths.defaultMountRoot : (volume.mountpoint as NSString).deletingLastPathComponent,
            name: volume.mountFolderName.isEmpty ? nil : volume.mountFolderName)
        request.mediaUUID = physicalPartition(for: volume)?.mediaUUID
        do {
            let remounted = try await bridge.mount(request)
            if volume.ghost.rules != .all {
                try? await bridge.ghostSetRules(volumeID: remounted.id, rules: volume.ghost.rules)
            }
            await refresh(force: true)
            selection = .volume(remounted.id)
        } catch {
            await refresh(force: true)
            present(L("Il volume è stato smontato ma non rimontato", "The volume was unmounted but not remounted"), error)
        }
        await refreshShadowSessions()
    }

    private func patchVolume(_ id: String, _ change: (inout VolumeStatus) -> Void) {
        guard let index = volumes.firstIndex(where: { $0.id == id }) else { return }
        change(&volumes[index])
    }

    func setGhostShield(_ volume: VolumeStatus, enabled: Bool) async {
        patchVolume(volume.id) { $0.ghost.enabled = enabled }
        do {
            try await bridge.ghostSetEnabled(volumeID: volume.id, enabled: enabled)
        } catch {
            present(L("Impossibile aggiornare Ghost Shield", "Could not update Ghost Shield"), error)
        }
        await refresh(force: true)
    }

    func setGhostRules(_ volume: VolumeStatus, rules newRules: GhostRules) async {
        patchVolume(volume.id) { $0.ghost.rules = newRules }
        do {
            try await bridge.ghostSetRules(volumeID: volume.id, rules: newRules)
        } catch {
            present(L("Impossibile aggiornare le regole di Ghost Shield", "Could not update the Ghost Shield rules"), error)
        }
        await refresh(force: true)
    }

    func purgeGhostVault(_ volume: VolumeStatus) async {
        do {
            try await bridge.ghostPurge(volumeID: volume.id)
        } catch {
            present(L("Impossibile svuotare il vault", "Could not empty the vault"), error)
        }
        await refresh(force: true)
    }

    func ghostEntries(_ volume: VolumeStatus) async -> [GhostEntry] {
        (try? await bridge.ghostEntries(volumeID: volume.id)) ?? []
    }

    func setUIDMap(_ volume: VolumeStatus, enabled: Bool, linuxUID: UInt32, linuxGID: UInt32) async {
        patchVolume(volume.id) {
            $0.uidMap.enabled = enabled
            $0.uidMap.linuxUid = linuxUID
            $0.uidMap.linuxGid = linuxGID
        }
        do {
            try await bridge.setUIDMap(volumeID: volume.id, linuxUID: linuxUID, linuxGID: linuxGID, enabled: enabled)
        } catch {
            present(L("Impossibile aggiornare la mappatura permessi", "Could not update the permission mapping"), error)
        }
        await refresh(force: true)
    }

    // MARK: Shadow Mount

    func shadowChanges(_ volume: VolumeStatus) async throws -> [ShadowChange] {
        try await bridge.shadowChanges(volumeID: volume.id)
    }

    func commitShadow(_ volume: VolumeStatus) async -> Bool {
        setActivity(volume.id, L("Applicazione delle modifiche…", "Applying changes…"))
        defer { setActivity(volume.id, nil) }
        do {
            try await bridge.shadowCommit(volumeID: volume.id)
            await refresh(force: true)
            await refreshShadowSessions()
            return true
        } catch {
            // A commit that stops half-way leaves the volume in "committing" state: show it.
            await refresh(force: true)
            await refreshShadowSessions()
            present(L("Impossibile applicare le modifiche", "Could not apply the changes"), error)
            return false
        }
    }

    /// Discards the pending changes; after an interrupted commit it abandons only the steps not
    /// yet applied (the applied ones stay on the disk).
    func rollbackShadow(_ volume: VolumeStatus) async -> Bool {
        setActivity(volume.id, volume.isShadowCommitInterrupted ? L("Abbandono delle modifiche rimanenti…", "Abandoning the remaining changes…")
                                                                : L("Annullamento delle modifiche…", "Discarding changes…"))
        defer { setActivity(volume.id, nil) }
        do {
            try await bridge.shadowRollback(volumeID: volume.id)
            await refresh(force: true)
            await refreshShadowSessions()
            return true
        } catch {
            await refresh(force: true)
            present(L("Impossibile scartare le modifiche", "Could not discard the changes"), error)
            return false
        }
    }

    // MARK: Saved Shadow sessions

    /// Reloads `shadowSessions`, then offers the recoverable ones (see `promptForSavedSessions`).
    func refreshShadowSessions(prompt: Bool = true) async {
        guard isReady, !isTerminating else { return }
        do {
            let latest = try await bridge.shadowSessions()
            if latest != shadowSessions { shadowSessions = latest }
            // A session mounted in this run is something the user is already working on: don't
            // offer it again after it is ejected (unless its state changes).
            for session in latest {
                if let id = session.mountedVolumeID, volume(id: id) != nil {
                    promptedSessions[session.key] = session.promptSignature
                }
            }
        } catch {
            NSLog("LumenDisk: ld_shadow_sessions failed: \(error.localizedDescription)")
            return
        }
        if prompt { promptForSavedSessions() }
    }

    /// Coalesces bursts of hot-plug events (a disk with several partitions) into one refresh.
    func scheduleShadowSessionsRefresh(after delay: Duration = .milliseconds(700)) {
        sessionsRefreshTask?.cancel()
        sessionsRefreshTask = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled, let self else { return }
            await self.refreshShadowSessions()
        }
    }

    /// The connected physical partition holding `session`: media UUID first, else the same
    /// device node with the same label. Nil for image sessions or when the disk isn't plugged in.
    func partition(for session: ShadowSession) -> DiskDescription? {
        guard let bsd = session.deviceBSDName else { return nil }
        let partitions = diskMonitor.disks.flatMap(\.partitions)
        let sessionMedia = session.mediaUUID.flatMap { $0.isEmpty ? nil : $0 }
        if let sessionMedia,
           let match = partitions.first(where: { $0.mediaUUID?.caseInsensitiveCompare(sessionMedia) == .orderedSame }) {
            return match
        }
        return partitions.first { partition in
            guard partition.bsdName == bsd else { return false }
            // Two different media UUIDs: a different partition that got the same node.
            if let sessionMedia, let media = partition.mediaUUID, !media.isEmpty,
               media.caseInsensitiveCompare(sessionMedia) != .orderedSame { return false }
            let labels = [partition.probedLabel, partition.volumeName].compactMap { $0 }.filter { !$0.isEmpty }
            if session.label.isEmpty { return labels.isEmpty }
            return labels.contains { $0.caseInsensitiveCompare(session.label) == .orderedSame }
        }
    }

    /// The saved session of a physical partition, if any.
    func shadowSession(for partition: DiskDescription) -> ShadowSession? {
        shadowSessions.first { $0.isPhysical && self.partition(for: $0)?.bsdName == partition.bsdName }
    }

    /// The saved session of an image file partition, if any.
    func shadowSession(imagePath: String, partition: UInt32) -> ShadowSession? {
        shadowSessions.first { !$0.isPhysical && !$0.isImageURL && !$0.isRemote && $0.sourcePath == imagePath && $0.partition == partition }
    }

    /// The volume this session is mounted as right now.
    func mountedVolume(for session: ShadowSession) -> VolumeStatus? {
        session.mountedVolumeID.flatMap { volume(id: $0) }
    }

    /// Whether the session's disk or image can be mounted right now.
    func isConnected(_ session: ShadowSession) -> Bool {
        if mountedVolume(for: session) != nil { return true }
        // Servers: only a new Shadow connection (with its credentials) can check and resume them.
        if session.isRemote { return false }
        if session.isPhysical { return partition(for: session) != nil }
        return session.connected
    }

    func canResume(_ session: ShadowSession) -> Bool {
        !session.isStale && mountedVolume(for: session) == nil && isConnected(session)
    }

    /// Activity key used while a session is being resumed or discarded.
    func activityKey(for session: ShadowSession) -> String {
        if let partition = partition(for: session) { return partition.bsdName }
        return session.isPhysical || session.isImageURL || session.isRemote ? "session:" + session.key : "\(session.sourcePath)#\(session.partition)"
    }

    /// Mounts the session's source in Shadow mode: the core restores the saved changes, or
    /// finishes an interrupted commit.
    @discardableResult
    func resumeShadowSession(_ session: ShadowSession) async -> VolumeStatus? {
        if let mounted = mountedVolume(for: session) {
            shadowFocusID = mounted.id
            selection = .shadow
            return mounted
        }
        promptedSessions[session.key] = session.promptSignature
        if session.isRemote {
            present(L("Connettiti al server in Shadow Mode", "Connect to the server in Shadow Mode"),
                    message: ShadowWording.remoteResumeHint(session))
            return nil
        }
        if session.isImageURL {
            let key = activityKey(for: session)
            setActivity(key, L("Montaggio…", "Mounting…"))
            defer { setActivity(key, nil) }
            do {
                return try await mountImageURL(ImageURLRequest(url: session.sourcePath, partition: session.partition, mode: .shadow))
            } catch {
                present(L("Impossibile riprendere le modifiche Shadow", "Could not resume the Shadow changes"), error)
                await refreshShadowSessions()
                return nil
            }
        }
        if session.isPhysical {
            guard let partition = partition(for: session) else {
                present(L("Disco non collegato", "Disk not connected"),
                        message: L("Collega «\(session.displayName)» per riprendere le modifiche Shadow salvate.",
                                   "Connect “\(session.displayName)” to resume the saved Shadow changes."))
                return nil
            }
            var options = rule(for: partition).map(MountOptions.init(rule:)) ?? MountOptions()
            options.mode = .shadow
            return await mountPhysical(partition, options: options, expectSession: session.key)
        }
        let rule = rules.rule(matching: [session.uuid.isEmpty ? nil : session.uuid, session.sourcePath])
        var request = MountRequest(path: session.sourcePath, partition: session.partition, mode: .shadow)
        if let rule {
            request.ghostShield = rule.ghostShield
            request.uidMap = rule.uidMapRequest
        }
        request.expectSession = session.key
        let volume = await mount(request, activityKey: activityKey(for: session))
        if volume != nil { rememberRecent(URL(fileURLWithPath: session.sourcePath)) }
        return volume
    }

    /// Deletes a saved session (the caller has already asked for confirmation). While a commit
    /// is interrupted only the steps not yet applied are abandoned.
    @discardableResult
    func discardShadowSession(_ session: ShadowSession) async -> Bool {
        let key = activityKey(for: session)
        setActivity(key, session.isCommitInterrupted ? L("Abbandono delle modifiche rimanenti…", "Abandoning the remaining changes…")
                                                     : L("Eliminazione delle modifiche…", "Deleting the changes…"))
        defer { setActivity(key, nil) }
        do {
            try await bridge.discardShadowSession(key: session.key)
            promptedSessions[session.key] = nil
            if mountedVolume(for: session) != nil { await refresh(force: true) }
            await refreshShadowSessions()
            return true
        } catch {
            await refreshShadowSessions()
            present(L("Impossibile scartare le modifiche", "Could not discard the changes"), error)
            return false
        }
    }

    // MARK: Recovery prompt

    private enum RecoveryChoice { case resume, discard, later }

    private var shadowPromptsDisabled: Bool {
        #if DEBUG
        ProcessInfo.processInfo.environment["LUMENDISK_NO_SHADOW_PROMPT"] == "1"
        #else
        false
        #endif
    }

    /// Offers, one at a time, the saved sessions whose disk or image is connected but not mounted.
    /// Each session is offered once per run unless its state changes.
    private func promptForSavedSessions() {
        guard isReady, !isTerminating, !isPromptingSessions, !shadowPromptsDisabled else { return }
        let next = shadowSessions.first { session in
            promptedSessions[session.key] != session.promptSignature
                && mountedVolume(for: session) == nil
                && isConnected(session)
                && !isBusy(activityKey(for: session))
                && (session.changeCount > 0 || session.isCommitInterrupted || session.isStale)
        }
        guard let session = next else { return }
        promptedSessions[session.key] = session.promptSignature
        isPromptingSessions = true
        Task { @MainActor [weak self] in
            // Let the window finish appearing and DiskArbitration settle before a modal alert.
            try? await Task.sleep(for: .milliseconds(300))
            guard let self else { return }
            let choice = self.runRecoveryPrompt(for: session)
            self.isPromptingSessions = false
            switch choice {
            case .resume: await self.resumeShadowSession(session)
            case .discard: await self.discardShadowSession(session)
            case .later: break
            }
            self.promptForSavedSessions()
        }
    }

    /// NSAlert asking what to do with a saved session. Discard is confirmed before returning.
    private func runRecoveryPrompt(for session: ShadowSession) -> RecoveryChoice {
        let canResume = !session.isStale
        while true {
            let alert = NSAlert()
            alert.alertStyle = canResume && !session.isCommitInterrupted ? .informational : .warning
            alert.messageText = ShadowWording.promptTitle(for: session)
            alert.informativeText = ShadowWording.promptMessage(for: session)
            var choices: [RecoveryChoice] = []
            if canResume {
                alert.addButton(withTitle: session.isCommitInterrupted ? L("Riprendi applicazione", "Resume applying") : L("Riprendi", "Resume"))
                choices.append(.resume)
            }
            let discard = alert.addButton(withTitle: session.isCommitInterrupted ? L("Abbandona il resto…", "Abandon the rest…") : L("Scarta…", "Discard…"))
            discard.hasDestructiveAction = true
            discard.keyEquivalent = ""   // never the default button
            choices.append(.discard)
            let later = alert.addButton(withTitle: L("Più tardi", "Later"))
            later.keyEquivalent = "\u{1b}"
            choices.append(.later)

            NSApp.activate()
            let index = alert.runModal().rawValue - NSApplication.ModalResponse.alertFirstButtonReturn.rawValue
            let choice = choices.indices.contains(index) ? choices[index] : .later
            if choice != .discard || confirmDiscard(session) { return choice }
        }
    }

    private func confirmDiscard(_ session: ShadowSession) -> Bool {
        let alert = NSAlert()
        alert.alertStyle = .warning
        if session.isCommitInterrupted {
            alert.messageText = ShadowWording.abandonTitle(name: session.displayName)
            alert.informativeText = ShadowWording.abandonMessage(applied: session.commitApplied, total: session.commitTotal)
        } else {
            alert.messageText = ShadowWording.discardTitle(name: session.displayName)
            alert.informativeText = ShadowWording.discardMessage(changes: session.changeCount)
        }
        let confirm = alert.addButton(withTitle: session.isCommitInterrupted ? ShadowWording.abandonButton
                                                                             : ShadowWording.discardButton(changes: session.changeCount))
        confirm.hasDestructiveAction = true
        confirm.keyEquivalent = ""
        let cancel = alert.addButton(withTitle: L("Annulla", "Cancel"))
        cancel.keyEquivalent = "\u{1b}"
        return alert.runModal() == .alertFirstButtonReturn
    }

    // MARK: Lock inspector

    func lockHolders(mountpoint: String) async throws -> [LockHolder] {
        try await bridge.lockHolders(mountpoint: mountpoint)
    }

    func inspectLocks(_ volume: VolumeStatus) {
        lockInspection = LockInspection(volumeID: volume.id, mountpoint: volume.mountpoint,
                                        volumeName: volume.displayName, failureMessage: nil)
    }

    // MARK: Journal doctor

    func repair(_ volume: VolumeStatus) async {
        setActivity(volume.id, L("Riparazione in corso…", "Repairing…"))
        defer { setActivity(volume.id, nil) }
        do {
            let report = try await bridge.repairVolume(volumeID: volume.id)
            await refresh(force: true)
            present(L("Volume riparato", "Volume repaired"),
                    message: report.isEmpty ? L("«\(volume.displayName)» è di nuovo scrivibile.", "“\(volume.displayName)” is writable again.") : report)
        } catch {
            present(L("Riparazione non riuscita", "Repair failed"), error)
        }
    }

    /// Repair an unmounted source (image partition or physical partition).
    func repairSource(path: String, partition: UInt32, activityKey: String) async -> Bool {
        setActivity(activityKey, L("Riparazione in corso…", "Repairing…"))
        defer { setActivity(activityKey, nil) }
        do {
            if let bsd = DeviceNode.bsdName(fromPath: path), let disk = diskMonitor.partition(bsdName: bsd), disk.isMountedByMacOS {
                try await diskMonitor.unmountNative(bsdName: bsd)
            }
            let report = try await bridge.repairSource(path: path, partition: partition)
            present(L("Riparazione completata", "Repair complete"),
                    message: report.isEmpty ? L("Il volume è pronto per la scrittura.", "The volume is ready for writing.") : report)
            return true
        } catch {
            present(L("Riparazione non riuscita", "Repair failed"), error)
            return false
        }
    }

    func inspect(path: String) async throws -> SourceInspection {
        try await bridge.inspect(path: path)
    }

    // MARK: Disk images

    func openImage(_ url: URL) {
        selection = .images
        imageSession = ImageSession(url: url)
        rememberRecent(url)
        Task {
            var session = ImageSession(url: url)
            do {
                let inspection = try await bridge.inspect(path: url.path)
                session.inspection = inspection
                session.isInspecting = false
                for partition in inspection.partitions {
                    let rule = rules.rule(matching: [partition.uuid, url.path])
                    let alreadyMounted = isMounted(path: url.path, partition: partition.index)
                    // Saved Shadow changes can only be reopened in Shadow mode (the core
                    // refuses a read/write mount and restores them in Shadow mode).
                    let hasSavedSession = shadowSession(imagePath: url.path, partition: partition.index) != nil
                    session.choices[partition.index] = .init(
                        selected: partition.isMountable && !alreadyMounted,
                        mode: hasSavedSession ? .shadow : (rule?.mode ?? .readWrite),
                        ruleName: rule?.displayName)
                    if let rule {
                        session.ghostShield = rule.ghostShield
                        session.uidMapEnabled = rule.uidMapEnabled
                    }
                }
            } catch {
                session.isInspecting = false
                session.error = error.localizedDescription
            }
            if imageSession?.url == url { imageSession = session }
        }
    }

    func isMounted(path: String, partition: UInt32) -> Bool {
        volumes.contains { $0.sourcePath == path && $0.partition == partition }
    }

    func mountSelectedImagePartitions() async {
        guard var session = imageSession, let inspection = session.inspection else { return }
        session.isMounting = true
        imageSession = session
        var lastMounted: VolumeStatus?
        for partition in inspection.partitions {
            guard let choice = session.choices[partition.index], choice.selected, partition.isMountable else { continue }
            let rule = rules.rule(matching: [partition.uuid, session.url.path])
            let uidMap = rule.map(\.uidMapRequest) ?? UIDMapRequest(enabled: session.uidMapEnabled, linuxUid: nil, linuxGid: nil)
            let request = MountRequest(path: session.url.path, partition: partition.index, mode: choice.mode,
                                       ghostShield: rule?.ghostShield ?? session.ghostShield, uidMap: uidMap)
            if let volume = await mount(request, activityKey: "\(session.url.path)#\(partition.index)", select: false) {
                lastMounted = volume
                rules.addAliases([session.url.path], toRuleWithVolumeUUID: volume.uuid, sourcePath: session.url.path)
            }
        }
        if var current = imageSession, current.url == session.url {
            current.isMounting = false
            for (index, choice) in current.choices where choice.selected && isMounted(path: session.url.path, partition: index) {
                current.choices[index]?.selected = false
            }
            imageSession = current
        }
        if let lastMounted { selection = .volume(lastMounted.id) }
    }

    func repairImagePartition(_ partition: PartitionInfo) async {
        guard let url = imageSession?.url else { return }
        imageSession?.repairing.insert(partition.index)
        let ok = await repairSource(path: url.path, partition: partition.index, activityKey: "\(url.path)#\(partition.index)")
        imageSession?.repairing.remove(partition.index)
        if ok, imageSession?.url == url, let refreshed = try? await bridge.inspect(path: url.path) {
            imageSession?.inspection = refreshed
        }
    }

    /// Used by the creation wizard: mount every mountable partition of a fresh image read/write.
    func mountFreshImage(path: String) async {
        do {
            let inspection = try await bridge.inspect(path: path)
            rememberRecent(URL(fileURLWithPath: path))
            guard let partition = inspection.partitions.first(where: \.isMountable) ?? inspection.partitions.first else {
                present(L("Nessun volume da montare", "Nothing to mount"),
                        message: L("L'immagine appena creata non contiene un file system riconosciuto.", "The new image does not contain a recognised file system."))
                return
            }
            await mount(MountRequest(path: path, partition: partition.index, mode: .readWrite), activityKey: path)
        } catch {
            present(L("Impossibile montare il disco virtuale", "Could not mount the virtual disk"), error)
        }
    }

    func createImage(_ request: CreateImageRequest) async throws -> String {
        try await bridge.createImage(request)
    }

    private func rememberRecent(_ url: URL) {
        var list = recentImages.filter { $0 != url.path }
        list.insert(url.path, at: 0)
        recentImages = Array(list.prefix(8))
        Preferences.shared.set(recentImages, forKey: "recentImages")
    }

    func forgetRecent(_ path: String) {
        recentImages.removeAll { $0 == path }
        Preferences.shared.set(recentImages, forKey: "recentImages")
    }

    // MARK: Rules

    func makeRule(from volume: VolumeStatus) -> AutomountRule {
        if let existing = rule(for: volume) { return existing }
        let partition = physicalPartition(for: volume)
        var rule = AutomountRule(volumeUUID: volume.uuid.isEmpty ? (partition?.mediaUUID ?? volume.sourcePath) : volume.uuid,
                                 displayName: volume.displayName, fsType: volume.fsType)
        rule.aliases = [partition?.mediaUUID, partition?.volumeUUID, volume.isPhysical ? nil : volume.sourcePath].compactMap { $0 }
        rule.mode = volume.mode
        rule.ghostShield = volume.ghost.enabled
        rule.uidMapEnabled = volume.uidMap.enabled
        rule.linuxUid = volume.uidMap.linuxUid
        rule.linuxGid = volume.uidMap.linuxGid
        rule.customIDs = volume.uidMap.linuxUid != 1000 || volume.uidMap.linuxGid != 1000
        rule.lastSourcePath = volume.sourcePath
        rule.automount = volume.isPhysical
        rules.upsert(rule)
        return rule
    }

    // MARK: Finder

    func openInFinder(_ volume: VolumeStatus) {
        let url = URL(fileURLWithPath: volume.mountpoint, isDirectory: true)
        guard FileManager.default.fileExists(atPath: url.path) else {
            present(L("Cartella non disponibile", "Folder not available"),
                    message: L("Il punto di montaggio \(volume.mountpoint) non è raggiungibile.", "The mount point \(volume.mountpoint) cannot be reached."))
            return
        }
        // Volumes in ~/Volumes are not listed by Finder on their own: if opening the folder
        // fails, reveal and select it in a Finder window instead.
        if !NSWorkspace.shared.open(url) {
            NSWorkspace.shared.activateFileViewerSelecting([url])
        }
    }

    // MARK: Remote storage (Phase 2)

    @discardableResult
    func mountRemote(_ request: RemoteMountRequest, select: Bool = true) async throws -> VolumeStatus {
        let volume = try await bridge.mountRemote(request)
        await refresh(force: true)
        if select { selection = .volume(volume.id) }
        return volume
    }

    func probeRemote(_ request: RemoteMountRequest) async throws -> ProbeResult {
        try await bridge.probeRemote(request)
    }

    /// Connects to a Mac discovered over Bonjour.
    @discardableResult
    func connect(to peer: LANPeer, pairingCode: String, mode: MountMode) async throws -> VolumeStatus {
        let url = try await lanDiscovery.baseURL(for: peer)
        let request = RemoteMountRequest(kind: .lan, url: url, pairingCode: pairingCode, fingerprint: peer.fingerprint,
                                         mode: mode, name: peer.name)
        return try await mountRemote(request)
    }

    func syncNow(_ volume: VolumeStatus) async {
        setActivity(volume.id, L("Sincronizzazione…", "Syncing…"))
        defer { setActivity(volume.id, nil) }
        do {
            let result = try await bridge.remoteSyncNow(volumeID: volume.id)
            await refresh(force: true)
            if let pending = result.pending, pending > 0 {
                present(L("Sincronizzazione incompleta", "Sync incomplete"),
                        message: L10n.plural(pending,
                                             one: ("%d file è ancora in attesa: il server non è raggiungibile. Resta al sicuro su questo Mac e verrà inviato appena possibile.",
                                                   "%d file is still waiting: the server can’t be reached. It stays safe on this Mac and will be sent as soon as possible."),
                                             other: ("%d file sono ancora in attesa: il server non è raggiungibile. Restano al sicuro su questo Mac e verranno inviati appena possibile.",
                                                     "%d files are still waiting: the server can’t be reached. They stay safe on this Mac and will be sent as soon as possible.")))
            }
        } catch {
            await refresh(force: true)
            present(L("Sincronizzazione non riuscita", "Sync failed"), error)
        }
    }

    func inspectImageURL(_ request: ImageURLRequest) async throws -> ImageURLInspection {
        try await bridge.inspectImageURL(request)
    }

    @discardableResult
    func mountImageURL(_ request: ImageURLRequest) async throws -> VolumeStatus {
        let volume = try await bridge.mountImageURL(request)
        await refresh(force: true)
        selection = .volume(volume.id)
        await refreshShadowSessions()
        return volume
    }

    // MARK: LAN share & Wi-Fi portal

    func setSharing(_ volume: VolumeStatus, enabled: Bool) async {
        setActivity(volume.id, enabled ? L("Avvio condivisione…", "Starting sharing…") : L("Arresto condivisione…", "Stopping sharing…"))
        defer { setActivity(volume.id, nil) }
        do {
            if enabled {
                if let share = try await bridge.shareStart(volumeID: volume.id) { startedShares[volume.id] = share }
            } else {
                try await bridge.shareStop(volumeID: volume.id)
                startedShares[volume.id] = nil
            }
        } catch {
            if !enabled { startedShares[volume.id] = nil }
            present(enabled ? L("Impossibile condividere il volume", "Could not share the volume")
                            : L("Impossibile interrompere la condivisione", "Could not stop sharing"), error)
        }
        await refresh(force: true)
    }

    /// Starts the Wi-Fi portal if needed, then shows the QR sheet in the main window.
    func showPortal(for volume: VolumeStatus) async {
        if portal(for: volume) == nil {
            setActivity(volume.id, L("Avvio portale Wi-Fi…", "Starting Wi-Fi portal…"))
            defer { setActivity(volume.id, nil) }
            do {
                if let portal = try await bridge.portalStart(volumeID: volume.id) { startedPortals[volume.id] = portal }
                await refresh(force: true)
            } catch {
                present(L("Impossibile avviare il portale Wi-Fi", "Could not start the Wi-Fi portal"), error)
                return
            }
        }
        activeSheet = .portal(volume.id)
        bringMainWindowForward()
    }

    /// Explicit, insecure opt-in for phones that refuse the portal's self-signed certificate.
    func setPortalPlainHTTP(_ volumeID: String, enabled: Bool) async {
        do {
            if let portal = try await bridge.portalSetPlainHTTP(volumeID: volumeID, enabled: enabled) { startedPortals[volumeID] = portal }
        } catch {
            present(enabled ? L("Impossibile attivare l'indirizzo non cifrato", "Could not turn on the unencrypted address")
                            : L("Impossibile disattivare l'indirizzo non cifrato", "Could not turn off the unencrypted address"), error)
        }
        await refresh(force: true)
    }

    func stopPortal(_ volumeID: String) async {
        do {
            try await bridge.portalStop(volumeID: volumeID)
        } catch {
            present(L("Impossibile interrompere il portale Wi-Fi", "Could not stop the Wi-Fi portal"), error)
        }
        startedPortals[volumeID] = nil
        await refresh(force: true)
    }

    // MARK: Disk health & speed (Phase 3)

    func speedTest(for volume: VolumeStatus) -> SpeedTestRecord? { speedTests[volume.speedTestKey] }

    /// Runs `ld_benchmark` (≈3 s, off the main actor through CoreBridge) and remembers the result.
    func runSpeedTest(_ volume: VolumeStatus) async {
        let key = volume.speedTestKey
        guard speedTests[key]?.isRunning != true else { return }
        var record = speedTests[key] ?? SpeedTestRecord()
        record.isRunning = true
        record.startedAt = Date()
        record.error = nil
        speedTests[key] = record
        do {
            let result = try await bridge.benchmark(volumeID: volume.id)
            record.result = result
            record.date = Date()
        } catch {
            record.error = friendlySpeedTestMessage(error)
        }
        record.isRunning = false
        speedTests[key] = record
        await refresh(force: true)
    }

    private func friendlySpeedTestMessage(_ error: any Error) -> String {
        let lower = error.localizedDescription.lowercased()
        if lower.contains("no space") || lower.contains("os error 28") {
            return L("Non c'è abbastanza spazio libero sul volume per il test di velocità. Libera un po' di spazio e riprova.",
                     "There isn’t enough free space on the volume for the speed test. Free up some space and try again.")
        }
        if lower.contains("os error 5)") || lower.contains("input/output") {
            return L("Il disco ha dato un errore di lettura/scrittura durante il test. Controlla il cavo o il collegamento e riprova.",
                     "The disk reported a read/write error during the test. Check the cable or connection and try again.")
        }
        if lower.contains("read-only") || lower.contains("os error 30") || lower.contains("sola lettura") {
            return L("Il volume è in sola lettura: il test ha bisogno di scrivere un file temporaneo.",
                     "The volume is read-only: the test needs to write a temporary file.")
        }
        return friendlyMessage(error)
    }

    func diskHealth(bsdName: String) -> DiskHealthLoad? { diskHealthReports[bsdName] }

    /// Loads the hardware report once per BSD name (pass `force` to read it again).
    func loadDiskHealth(bsdName: String, force: Bool = false) async {
        if !force, let current = diskHealthReports[bsdName] {
            if case .failed = current {} else { return }   // already loading or loaded
        }
        diskHealthReports[bsdName] = .loading
        do {
            diskHealthReports[bsdName] = .loaded(try await bridge.diskHealth(bsdName: bsdName))
        } catch {
            diskHealthReports[bsdName] = .failed(friendlyMessage(error))
        }
    }
}
