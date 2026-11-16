import Foundation
import Observation

/// One forge job as the UI tracks it: the latest snapshot plus every log line received so far.
struct ForgeJob: Identifiable, Sendable {
    let id: UInt64
    let tool: ForgeCenter.Tool
    let kind: String
    let title: String
    let diskBSD: String?
    let diskTitle: String?
    var snapshot: ForgeSnapshot?
    var logs: [ForgeLogLine] = []
    var lastSeq: UInt64 = 0
    var cancelRequested = false
    /// The core stopped answering for this job (it should not happen: jobs live until forgotten).
    var lostError: String?

    var state: ForgeJobState {
        if lostError != nil { return .failed }
        return snapshot?.state ?? .running
    }

    var isRunning: Bool { state == .running }
    var errorMessage: String? { lostError ?? snapshot?.error }
}

/// A destructive operation waiting for the user's explicit confirmation.
struct ForgeConfirmation: Identifiable, Sendable {
    let id = UUID()
    let tool: ForgeCenter.Tool
    let request: ForgeRequest
    let disk: ForgeDisk
    /// Job title shown in the dashboard ("Formattazione di …").
    let jobTitle: String
    let headline: String
    let actionTitle: String
    /// What will happen, one line each.
    let details: [String]
    /// Erases the disk's contents (format, flash, wipe) rather than rewriting a table.
    let erasesData: Bool
}

/// State and jobs of the Forge area (Create & Format, checksums, secure wipe, partition
/// doctor). Owned by AppModel, so jobs keep running and being polled while the area — or the
/// whole window — is closed. Every FFI call goes through CoreBridge (off the main actor).
@MainActor
@Observable
final class ForgeCenter {
    enum Tool: String, CaseIterable, Identifiable, Sendable {
        case create, hash, wipe, doctor
        var id: String { rawValue }

        var title: String {
            switch self {
            case .create: L("Crea & Formatta", "Create & Format")
            case .hash: L("Checksum", "Hash & Integrity")
            case .wipe: L("Cancellazione Sicura", "Secure Wipe")
            case .doctor: L("Doctor Partizioni", "Partition Doctor")
            }
        }

        var symbol: String {
            switch self {
            case .create: "wand.and.rays"
            case .hash: "number.square"
            case .wipe: "eraser.line.dashed"
            case .doctor: "stethoscope"
            }
        }
    }

    @ObservationIgnored private let bridge = CoreBridge.shared

    var tool: Tool = .create

    // Disks
    private(set) var disks: [ForgeDisk] = []
    private(set) var isLoadingDisks = false
    private(set) var disksLoaded = false
    private(set) var disksError: String?
    @ObservationIgnored private var disksWanted = false
    @ObservationIgnored private var refreshAgain = false
    @ObservationIgnored private var refreshTask: Task<Void, Never>?
    @ObservationIgnored private var lastDiskRefresh = Date.distantPast

    // Create & Format
    var plan = ForgePlan()
    private(set) var inspection: ImageInspection?
    private(set) var inspectingPath: String?
    private(set) var inspectError: String?
    var drawerOpen = false
    var showAllSections = false
    /// Section the drawer scrolls to ("windows", "linux", "rpi"); set by the debug hooks.
    var drawerScrollTarget: String?
    var confirmation: ForgeConfirmation?
    var confirmationAcknowledged = false
    var logDrawerOpen = false

    // Utilities
    var hashDraft = HashDraft()
    var wipeTarget: String?
    var wipeMethod: WipeMethod = .zeros
    var wipeVerify = true
    var doctorTarget: String?

    // Jobs: at most one per tool.
    private(set) var jobs: [Tool: ForgeJob] = [:]
    /// Errors raised before a job could start (refused request…), per tool.
    var startErrors: [Tool: String] = [:]
    @ObservationIgnored private var pollers: [UInt64: Task<Void, Never>] = [:]

    // MARK: Derived

    var runningJobs: [ForgeJob] { Tool.allCases.compactMap { jobs[$0] }.filter(\.isRunning) }
    var hasRunningJobs: Bool { !runningJobs.isEmpty }

    func disk(_ bsd: String?) -> ForgeDisk? {
        guard let bsd else { return nil }
        return disks.first { $0.bsd == bsd }
    }

    var targetDisk: ForgeDisk? { disk(plan.targetBSD) }

    /// Live issues of the Create & Format setup (empty = ready to start).
    var createIssues: [ForgeIssue] {
        inspection == nil ? plan.formatIssues(disk: targetDisk) : plan.flashIssues(disk: targetDisk, inspection: inspection)
    }

    // MARK: Disks

    /// The Forge area became visible: from now on disk changes refresh the list.
    func activate() {
        disksWanted = true
        if !disksLoaded || Date().timeIntervalSince(lastDiskRefresh) > 20 {
            Task { await refreshDisks() }
        }
    }

    /// DiskArbitration saw a change: refresh soon (coalesced; `ld_forge_disks` runs `diskutil`
    /// for every disk, which takes a few seconds).
    func scheduleDiskRefresh() {
        guard disksWanted else { return }
        refreshTask?.cancel()
        refreshTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(1200))
            guard !Task.isCancelled else { return }
            await self?.refreshDisks()
        }
    }

    /// Reloads the list; if a reload is already running, asks for one more pass and waits for it.
    func refreshDisks() async {
        guard !isLoadingDisks else {
            refreshAgain = true
            while isLoadingDisks { try? await Task.sleep(for: .milliseconds(100)) }
            return
        }
        isLoadingDisks = true
        repeat {
            refreshAgain = false
            do {
                let list = try await bridge.forgeDisks()
                disks = ForgeDiskRules.sorted(list)
                disksError = nil
            } catch {
                disksError = error.localizedDescription
            }
        } while refreshAgain
        isLoadingDisks = false
        disksLoaded = true
        lastDiskRefresh = Date()
        dropUnselectableTargets()
    }

    /// A selected disk that disappeared or is no longer allowed is deselected.
    private func dropUnselectableTargets() {
        let ok: (String?) -> Bool = { [self] bsd in bsd.flatMap { disk($0) }.map(ForgeDiskRules.canSelect) ?? false }
        if plan.targetBSD != nil && !ok(plan.targetBSD) && jobs[.create]?.isRunning != true { plan.targetBSD = nil }
        if wipeTarget != nil && !ok(wipeTarget) && jobs[.wipe]?.isRunning != true { wipeTarget = nil }
        if doctorTarget != nil && !ok(doctorTarget) && jobs[.doctor]?.isRunning != true { doctorTarget = nil }
        if case .disk(let bsd)? = hashDraft.source, !ok(bsd), jobs[.hash]?.isRunning != true { hashDraft.source = nil }
    }

    /// Selects `disk` as the destination of `tool`. Refused disks are never selected.
    @discardableResult
    func select(_ disk: ForgeDisk, for tool: Tool) -> Bool {
        guard ForgeDiskRules.canSelect(disk) else { return false }
        switch tool {
        case .create: plan.targetBSD = disk.bsd
        case .hash: hashDraft.source = .disk(disk.bsd)
        case .wipe: wipeTarget = disk.bsd
        case .doctor: doctorTarget = disk.bsd
        }
        return true
    }

    func selectedBSD(for tool: Tool) -> String? {
        switch tool {
        case .create: plan.targetBSD
        case .hash: if case .disk(let bsd)? = hashDraft.source { bsd } else { nil }
        case .wipe: wipeTarget
        case .doctor: doctorTarget
        }
    }

    // MARK: Image

    func loadImage(_ url: URL) async {
        let path = url.path
        inspectingPath = path
        inspectError = nil
        defer { if inspectingPath == path { inspectingPath = nil } }
        do {
            let result = try await bridge.forgeInspect(path: path)
            guard inspectingPath == path else { return }   // another image was dropped meanwhile
            inspection = result
            plan.mode = .auto
            plan.rpi.enabled = result.rpi != nil
            plan.expectedChecksum = ""
            if let label = result.iso?.label, !label.isEmpty, plan.linux.label.isEmpty { plan.linux.label = label }
            NSLog("Forge: inspected \(result.fileName): \(result.format), mode \(result.recommendedMode.rawValue)")
        } catch {
            guard inspectingPath == path else { return }
            inspectError = error.localizedDescription
        }
    }

    func clearImage() {
        inspection = nil
        inspectError = nil
        inspectingPath = nil
    }

    // MARK: Starting jobs

    /// "Format…" / "Write…": builds the request from the current setup and asks for confirmation.
    @discardableResult
    func prepareCreate() -> Bool {
        let built = inspection == nil ? plan.formatRequest(disk: targetDisk) : plan.flashRequest(disk: targetDisk, inspection: inspection)
        guard case .success(let request) = built, let disk = targetDisk else { return false }
        var details: [String] = []
        let jobTitle: String
        let action: String
        if let inspection {
            let mode = plan.effectiveMode(inspection)
            jobTitle = L("Scrittura di \(inspection.fileName)", "Writing \(inspection.fileName)")
            action = L("Cancella e Scrivi", "Erase and Write")
            details.append(L("Scrive «\(inspection.fileName)» in modalità \(mode.title).", "Writes “\(inspection.fileName)” in \(mode.title) mode."))
            details.append(plan.verify ? L("Poi rilegge il disco e lo confronta con l'immagine.", "Then reads the disk back and compares it with the image.")
                                       : L("Senza verifica finale.", "Without a final verification."))
            if plan.eject { details.append(L("Alla fine espelle il disco.", "Ejects the disk at the end.")) }
        } else {
            jobTitle = L("Formattazione di \(disk.title)", "Formatting \(disk.title)")
            action = L("Cancella e Formatta", "Erase and Format")
            let p1 = plan.primary
            var line = L("Crea una tabella \(plan.scheme.title) con \(p1.fs.displayName)", "Creates a \(plan.scheme.title) table with \(p1.fs.displayName)")
            let label = p1.fs.sanitizedLabel(p1.label)
            if !label.isEmpty { line += " «\(label)»" }
            if plan.scheme == .none { line = L("Crea \(p1.fs.displayName) sull'intero disco, senza tabella delle partizioni", "Creates \(p1.fs.displayName) on the whole disk, without a partition table") }
            details.append(line + ".")
            if plan.addSecond && plan.scheme != .none {
                details.append(L("Seconda partizione \(plan.secondary.fs.displayName) nel resto del disco.", "Second \(plan.secondary.fs.displayName) partition in the rest of the disk."))
            }
        }
        confirmation = ForgeConfirmation(tool: .create, request: request, disk: disk, jobTitle: jobTitle,
                                         headline: L("Cancellare tutto su «\(disk.title)»?", "Erase everything on “\(disk.title)”?"),
                                         actionTitle: action, details: details, erasesData: true)
        confirmationAcknowledged = false
        return true
    }

    @discardableResult
    func prepareWipe() -> Bool {
        guard let disk = disk(wipeTarget), ForgeDiskRules.canSelect(disk) else { return false }
        let request = ForgeRequest.wipe(WipeRequest(target: .disk(bsd: disk.bsd), method: wipeMethod, verify: wipeVerify))
        let estimate = WipeEstimate.seconds(size: disk.size, method: wipeMethod, verify: wipeVerify, bytesPerSecond: ForgeDiskRules.nominalWriteSpeed(disk))
        confirmation = ForgeConfirmation(
            tool: .wipe, request: request, disk: disk,
            jobTitle: L("Cancellazione sicura di \(disk.title)", "Secure wipe of \(disk.title)"),
            headline: L("Cancellare in modo sicuro «\(disk.title)»?", "Securely wipe “\(disk.title)”?"),
            actionTitle: L("Cancella in Modo Sicuro", "Wipe Securely"),
            details: [wipeMethod.title + ".", L("Tempo stimato: circa \(ForgeFormat.duration(seconds: estimate)).", "Estimated time: about \(ForgeFormat.duration(seconds: estimate)).")],
            erasesData: true)
        confirmationAcknowledged = false
        return true
    }

    /// Doctor: a check starts at once (read-only); repair and conversions ask first.
    @discardableResult
    func prepareDoctor(_ action: DoctorAction) -> Bool {
        guard let disk = disk(doctorTarget), ForgeDiskRules.canSelect(disk) else { return false }
        let request = ForgeRequest.doctor(DoctorRequest(target: .disk(bsd: disk.bsd), action: action))
        let title = action.title + " · " + disk.title
        if action == .check {
            Task { await start(request, tool: .doctor, title: title, disk: disk) }
            return true
        }
        let detail: String = switch action {
        case .repair: L("Riscrive solo le strutture della tabella delle partizioni danneggiate (dalla loro copia di riserva). I dati nelle partizioni non vengono toccati.",
                        "Rewrites only the damaged partition table structures (from their backup copy). The data in the partitions is not touched.")
        case .convertToGpt: L("Converte la tabella MBR in GPT mantenendo le partizioni al loro posto.", "Converts the MBR table to GPT, keeping the partitions where they are.")
        case .convertToMbr: L("Converte la tabella GPT in MBR mantenendo le partizioni al loro posto (al massimo 4, entro 2 TiB).", "Converts the GPT table to MBR, keeping the partitions where they are (at most 4, within 2 TiB).")
        case .check: ""
        }
        confirmation = ForgeConfirmation(
            tool: .doctor, request: request, disk: disk, jobTitle: title,
            headline: L("\(action.title) la tabella delle partizioni di «\(disk.title)»?", "\(action.title) the partition table of “\(disk.title)”?"),
            actionTitle: action.title, details: [detail, L("Il disco verrà smontato.", "The disk will be unmounted.")], erasesData: false)
        confirmationAcknowledged = false
        return true
    }

    /// The confirmation sheet's destructive button.
    func confirmPending() async {
        guard let c = confirmation, confirmationAcknowledged else { return }
        confirmation = nil
        confirmationAcknowledged = false
        // The disk must still be one the policy allows (it could have changed meanwhile).
        guard let current = disk(c.disk.bsd), ForgeDiskRules.canSelect(current) else {
            startErrors[c.tool] = L("Il disco non è più disponibile.", "The disk is no longer available.")
            return
        }
        await start(c.request, tool: c.tool, title: c.jobTitle, disk: current)
    }

    func startHash() async {
        guard case .success(let request) = hashDraft.request() else { return }
        let name: String = switch hashDraft.source {
        case .file(let path)?: (path as NSString).lastPathComponent
        case .disk(let bsd)?: disk(bsd)?.title ?? bsd
        case nil: ""
        }
        await start(request, tool: .hash, title: L("Checksum di \(name)", "Checksum of \(name)"), disk: request.diskBSD.flatMap { disk($0) })
    }

    func start(_ request: ForgeRequest, tool: Tool, title: String, disk: ForgeDisk?) async {
        guard jobs[tool]?.isRunning != true else { return }
        startErrors[tool] = nil
        if let old = jobs[tool] { await forget(old) }
        do {
            let id = try await bridge.forgeStart(request)
            jobs[tool] = ForgeJob(id: id, tool: tool, kind: request.op, title: title, diskBSD: disk?.bsd, diskTitle: disk?.title)
            logDrawerOpen = false
            NSLog("Forge: started job \(id) (\(request.op)) \(title)")
            track(id, tool: tool)
        } catch {
            startErrors[tool] = ForgeFormat.friendlyError(error.localizedDescription)
        }
    }

    // MARK: Running jobs

    private func track(_ id: UInt64, tool: Tool) {
        pollers[id]?.cancel()
        pollers[id] = Task { [weak self] in
            var failures = 0
            while !Task.isCancelled {
                guard let self, let job = self.jobs[tool], job.id == id else { return }
                do {
                    let snapshot = try await self.bridge.forgeStatus(job: id, since: job.lastSeq)
                    failures = 0
                    if self.apply(snapshot, tool: tool) { return }
                } catch {
                    failures += 1
                    if failures >= 12 {
                        self.jobs[tool]?.lostError = error.localizedDescription
                        self.pollers[id] = nil
                        return
                    }
                }
                try? await Task.sleep(for: .milliseconds(250))
            }
        }
    }

    /// Stores a snapshot; true once the job has finished.
    private func apply(_ snapshot: ForgeSnapshot, tool: Tool) -> Bool {
        guard var job = jobs[tool], job.id == snapshot.id else { return true }
        var stored = snapshot
        stored.logs = []
        job.snapshot = stored
        if let last = snapshot.logs.last {
            job.logs.append(contentsOf: snapshot.logs)
            if job.logs.count > 5000 { job.logs.removeFirst(job.logs.count - 5000) }
            job.lastSeq = last.seq
        }
        jobs[tool] = job
        guard snapshot.state != .running else { return false }
        pollers[job.id] = nil
        NSLog("Forge: job \(job.id) (\(job.kind)) \(snapshot.state.rawValue)\(snapshot.error.map { ": \($0)" } ?? "")")
        if job.kind != "hash" || job.diskBSD != nil {
            scheduleDiskRefresh()
        }
        return true
    }

    func cancel(_ tool: Tool) async {
        guard let job = jobs[tool], job.isRunning else { return }
        jobs[tool]?.cancelRequested = true
        try? await bridge.forgeCancel(job: job.id)
    }

    /// "Done": the result was seen; the core can drop the job.
    func dismiss(_ tool: Tool) async {
        guard let job = jobs[tool], !job.isRunning else { return }
        jobs[tool] = nil
        await forget(job)
    }

    private func forget(_ job: ForgeJob) async {
        pollers[job.id]?.cancel()
        pollers[job.id] = nil
        try? await bridge.forgeForget(job: job.id)
    }

    /// Quit: ask every running job to stop, and wait (briefly) for them to do so.
    func cancelAllAndWait(timeout: Duration = .seconds(8)) async {
        for job in runningJobs { try? await bridge.forgeCancel(job: job.id) }
        let deadline = ContinuousClock.now + timeout
        while hasRunningJobs && ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(200))
        }
    }

    /// Waits until `tool`'s job has finished (debug hooks).
    func waitForJob(_ tool: Tool, timeout: Duration) async -> ForgeJob? {
        let deadline = ContinuousClock.now + timeout
        while let job = jobs[tool], job.isRunning, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(150))
        }
        return jobs[tool]
    }
}
