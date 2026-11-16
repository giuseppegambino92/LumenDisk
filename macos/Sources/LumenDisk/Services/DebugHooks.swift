#if DEBUG
import AppKit
import SwiftUI

/// Development-only hooks (compiled out of release builds) used to drive the app with real
/// data from the real core and take screenshots without accessibility access. Nothing here
/// fabricates state: every hook goes through AppModel → CoreBridge or DiskArbitration.
///
/// Launch-time environment:
///   LUMENDISK_LANG=<it|en>          interface language (not persisted)
///   LUMENDISK_DARK=1                dark appearance
///   LUMENDISK_SELECT=<spec>         see `select(_:)`
///   LUMENDISK_SHEET=<spec>          see `sheet(_:)`
///   LUMENDISK_TECH=1                "Dettagli tecnici" starts expanded
///   LUMENDISK_SCROLL=bottom         detail pages start scrolled to the bottom
///   LUMENDISK_WINDOW=<w>x<h>        main window size
///   LUMENDISK_POPOVER_WINDOW=1      menu bar popover in a panel
///   LUMENDISK_NO_BONJOUR=1          disables Bonjour browsing and publishing
///   LUMENDISK_NO_SHADOW_PROMPT=1    never show the saved-Shadow-session recovery prompt
///   LUMENDISK_MOUNT_ROOT=<dir>      parent folder for every mountpoint (see AppPaths)
///   LUMENDISK_MOUNT="<path>|<mode>|<partition>[|<name>];…"   mount real images at launch
///   LUMENDISK_IMAGEURL="<url>|<partition>[|<mode>]"         mount a real remote image (HTTP Range)
///   LUMENDISK_MOUNT_PHYSICAL=<mode> press "Monta Ora" on the first partition ready for LumenDisk
///   LUMENDISK_SHARE=<name>          share this volume on the LAN (LUMENDISK_SHARE_FIRST=1: the first)
///   LUMENDISK_LAN_LOOPBACK=1        then mount that share back as a LAN client (127.0.0.1)
///   LUMENDISK_PORTAL=<name>         start this volume's Wi-Fi portal (LUMENDISK_PORTAL_FIRST=1: the first)
///   LUMENDISK_SPEEDTEST="<name>,…"  run the real speed test on these volumes (display names)
///   LUMENDISK_TEST_PHYSICAL=1       mount + eject the first physical partition (e2e check)
///   LUMENDISK_READY_FILE=<path>     written when every launch hook above has finished
///   LUMENDISK_CONTROL=<path>        runtime command file, see `run(command:)`
///   LUMENDISK_QUIT_AFTER=<seconds>  eject everything and quit
@MainActor
enum DebugHooks {
    private static var panel: NSPanel?
    private static var lastCommandStamp: Date?
    private static var executedCommands = 0

    /// Applied before any window is created.
    static func applyEarly() {
        let env = ProcessInfo.processInfo.environment
        if let lang = env["LUMENDISK_LANG"].flatMap(AppLanguage.init(rawValue:)) {
            LanguageStore.shared.override(lang)
        }
        if env["LUMENDISK_DARK"] == "1" { NSApp.appearance = NSAppearance(named: .darkAqua) }
        if env["LUMENDISK_TECH"] == "1" { TechnicalDetailsCard.startExpanded = true }
        if env["LUMENDISK_SCROLL"] == "bottom" { DetailCanvasDebug.startAtBottom = true }
        ConnectServerSheet.debugPrefill = env["LUMENDISK_SHEET"] == "connect-filled"
        ImageURLSheet.debugPrefill = env["LUMENDISK_SHEET"] == "imageurl-filled"
    }

    static func apply(to model: AppModel) {
        let env = ProcessInfo.processInfo.environment
        if let size = env["LUMENDISK_WINDOW"]?.split(separator: "x").compactMap({ Double($0) }), size.count == 2 {
            Task {
                try? await Task.sleep(for: .milliseconds(300))
                resizeMainWindow(width: size[0], height: size[1])
            }
        }
        if env["LUMENDISK_POPOVER_WINDOW"] == "1" { showPopoverPanel(model) }

        // Everything that depends on real mounts runs in order, then selection and sheets.
        Task {
            await runLaunchHooks(model, env: env)
            if let spec = env["LUMENDISK_SELECT"] { await select(spec, model) }
            if let spec = env["LUMENDISK_SHEET"] { await sheet(spec, model) }
            if let path = env["LUMENDISK_READY_FILE"] {
                try? Data("ready\n".utf8).write(to: URL(fileURLWithPath: path))
            }
            NSLog("DEBUG: launch hooks done; volumes=\(model.volumes.map(\.displayName))")
            if let path = env["LUMENDISK_CONTROL"] { watchControlFile(path, model) }
        }

        if let seconds = env["LUMENDISK_QUIT_AFTER"].flatMap(Double.init) {
            Task {
                try? await Task.sleep(for: .seconds(seconds))
                await quit(model)
            }
        }
    }

    // MARK: Launch hooks (real core calls only)

    private static func runLaunchHooks(_ model: AppModel, env: [String: String]) async {
        if let spec = env["LUMENDISK_MOUNT"] {
            for item in spec.split(separator: ";") {
                let f = item.split(separator: "|").map(String.init)
                guard let path = f.first else { continue }
                let mode = f.count > 1 ? (MountMode(rawValue: f[1]) ?? .readWrite) : .readWrite
                let part = f.count > 2 ? (UInt32(f[2]) ?? 0) : 0
                let name = f.count > 3 ? f[3] : nil
                let v = await model.mount(MountRequest(path: path, partition: part, mode: mode, name: name), activityKey: path)
                NSLog("DEBUG: mounted \(path) -> \(v?.mountpoint ?? "FAILED: \(model.alert?.message ?? "-")")")
            }
        }
        if let spec = env["LUMENDISK_IMAGEURL"] {
            let f = spec.split(separator: "|").map(String.init)
            var request = ImageURLRequest(url: f[0])
            if f.count > 1 { request.partition = UInt32(f[1]) ?? 0 }
            if f.count > 2 { request.mode = MountMode(rawValue: f[2]) ?? .shadow }
            do {
                let v = try await model.mountImageURL(request)
                NSLog("DEBUG: image url mounted -> \(v.mountpoint)")
            } catch {
                NSLog("DEBUG: image url FAILED: \(error.localizedDescription)")
            }
        }
        if let physicalMode = env["LUMENDISK_MOUNT_PHYSICAL"] {
            await mountPhysical(mode: physicalMode, model)
        }
        if env["LUMENDISK_TEST_PHYSICAL"] == "1" {
            try? await Task.sleep(for: .seconds(1.5))
            if let p = model.unmanagedPartitions.first {
                let v = await model.mountPhysical(p, options: MountOptions())
                NSLog("DEBUG: result \(v?.id ?? "nil") alert=\(model.alert?.message ?? "-")")
                if let v { await model.eject(v) }
            }
        }
        let shareTarget = env["LUMENDISK_SHARE"].flatMap { volume(named: $0, model) }
            ?? (env["LUMENDISK_SHARE_FIRST"] == "1" ? model.localVolumes.first : nil)
        if let v = shareTarget {
            await model.setSharing(v, enabled: true)
            let share = model.volume(id: v.id).flatMap { model.share(for: $0) }
            NSLog("DEBUG: share \(share.map { "port \($0.port) code \($0.pairingCode) scheme \($0.scheme) fp \($0.fingerprint ?? "-")" } ?? "FAILED: \(model.alert?.message ?? "-")")")
            if env["LUMENDISK_LAN_LOOPBACK"] == "1", let share {
                await mountLoopback(share: share, name: v.displayName, model)
            }
        }
        let portalTarget = env["LUMENDISK_PORTAL"].flatMap { volume(named: $0, model) }
            ?? (env["LUMENDISK_PORTAL_FIRST"] == "1" ? model.localVolumes.first : nil)
        if let v = portalTarget {
            await model.showPortal(for: v)
            model.activeSheet = nil
        }
        if let names = env["LUMENDISK_SPEEDTEST"] {
            for name in names.split(separator: ",").map(String.init) {
                await speedTest(name, model)
            }
        }
    }

    /// Presses "Monta Ora" on the first partition that is ready for LumenDisk.
    private static func mountPhysical(mode: String, _ model: AppModel) async {
        // Wait for DiskArbitration (and LumenDisk's probe of unknown devices) to settle.
        for _ in 0..<40 where !model.unmanagedPartitions.contains(where: \.isReadyForLumenDisk) {
            try? await Task.sleep(for: .milliseconds(250))
        }
        guard let p = model.unmanagedPartitions.first(where: \.isReadyForLumenDisk) else {
            NSLog("DEBUG: no partition ready for LumenDisk")
            return
        }
        NSLog("DEBUG: Monta Ora on \(p.bsdName) (\(PhysicalPartitionStatus.headline(p)))")
        let v = await model.mountPhysical(p, options: MountOptions(mode: MountMode(rawValue: mode) ?? .readWrite), select: false)
        NSLog("DEBUG: physical mount -> \(v?.mountpoint ?? "FAILED: \(model.alert?.message ?? "-")")")
    }

    /// Mounts this Mac's own LAN share as a LAN client, exactly like PairingSheet would.
    private static func mountLoopback(share: ShareStatus, name: String, _ model: AppModel) async {
        let url = "\(share.scheme)://127.0.0.1:\(share.port)"
        let request = RemoteMountRequest(kind: .lan, url: url, pairingCode: share.pairingCode,
                                         fingerprint: share.fingerprint, mode: .readWrite, name: name + " (LAN)")
        do {
            let v = try await model.mountRemote(request, select: false)
            NSLog("DEBUG: LAN client \(url) -> \(v.mountpoint)")
        } catch {
            NSLog("DEBUG: LAN client FAILED: \(error.localizedDescription)")
        }
    }

    private static func volume(named name: String, _ model: AppModel) -> VolumeStatus? {
        if name == "physical" { return model.volumes.first(where: \.isPhysical) }
        return model.volumes.first { $0.displayName == name }
    }

    private static func speedTest(_ name: String, _ model: AppModel) async {
        guard let v = volume(named: name, model) else { NSLog("DEBUG: speedtest: no volume \(name)"); return }
        await model.runSpeedTest(v)
        let record = model.speedTest(for: v)
        NSLog("DEBUG: speedtest \(name): \(record?.result.map { "\($0.summary) · \($0.verdict.text) · \($0.randomSummary)" } ?? "FAILED: \(record?.error ?? "-")")")
    }

    // MARK: Selection & sheets

    private static func select(_ value: String, _ model: AppModel) async {
        switch value {
        case "shadow": model.selection = .shadow
        case "ghost": model.selection = .ghost
        case "images": model.selection = .images
        case "rules": model.selection = .rules
        case "forge": model.selection = .forge
        case _ where value.hasPrefix("forge:"):
            if let tool = ForgeCenter.Tool(rawValue: String(value.dropFirst(6))) { model.forge.tool = tool }
            model.selection = .forge
        case "network": model.selection = .network
        case _ where value.hasPrefix("peer"):
            let index = Int(value.dropFirst(4)) ?? 0
            if model.lanDiscovery.peers.indices.contains(index) { model.selection = .lanPeer(model.lanDiscovery.peers[index].id) }
        case _ where value.hasPrefix("vol"):
            let index = Int(value.dropFirst(3)) ?? 0
            if model.volumes.indices.contains(index) { model.selection = .volume(model.volumes[index].id) }
        case _ where value.hasPrefix("remote"):
            let index = Int(value.dropFirst(6)) ?? 0
            if model.remoteVolumes.indices.contains(index) { model.selection = .volume(model.remoteVolumes[index].id) }
        case _ where value.hasPrefix("name:"):
            if let v = volume(named: String(value.dropFirst(5)), model) { model.selection = .volume(v.id) }
        case _ where value.hasPrefix("focus-shadow:"):
            if let v = volume(named: String(value.dropFirst(13)), model) { model.shadowFocusID = v.id; model.selection = .shadow }
        case _ where value.hasPrefix("focus-ghost:"):
            if let v = volume(named: String(value.dropFirst(12)), model) { model.ghostFocusID = v.id; model.selection = .ghost }
        case _ where value.hasPrefix("image:"):
            model.openImage(URL(fileURLWithPath: String(value.dropFirst(6))))
        case "physical":
            for _ in 0..<20 where model.unmanagedPartitions.isEmpty { try? await Task.sleep(for: .milliseconds(250)) }
            if let p = model.unmanagedPartitions.first { model.selection = .physical(p.bsdName) }
        default: break
        }
    }

    private static func sheet(_ value: String, _ model: AppModel) async {
        switch value {
        case "close": model.activeSheet = nil; model.showCreateWizard = false; model.lockInspection = nil
        case "locks":
            if let v = currentVolume(model) { model.inspectLocks(v) }
        case "create": model.showCreateWizard = true
        case "connect", "connect-filled": model.activeSheet = .connectServer
        case "imageurl", "imageurl-filled": model.activeSheet = .imageURL
        case "pair":
            if let peer = model.lanDiscovery.peers.first { model.activeSheet = .pair(peer) }
        case "portal":
            if let target = currentVolume(model) { await model.showPortal(for: target) }
        default: break
        }
    }

    private static func currentVolume(_ model: AppModel) -> VolumeStatus? {
        if case .volume(let id)? = model.selection, let v = model.volume(id: id) { return v }
        return model.localVolumes.first ?? model.volumes.first
    }

    // MARK: Runtime control file

    /// Polls `path`; whenever it changes, runs each line as a command and appends
    /// "done <n>" to `<path>.ack`. Commands:
    ///   select <spec> · sheet <spec> · lang <it|en> · appearance <dark|light> · tech <on|off>
    ///   scroll <top|bottom> · speedtest <name> · popover <show|hide> · window <w>x<h>
    ///   mount-physical <mode> · eject <name> · refresh · quit
    ///   forge <verb> … (see `forge(_:_:)`): Create & Format and the disk utilities
    private static func watchControlFile(_ path: String, _ model: AppModel) {
        Task {
            while true {
                try? await Task.sleep(for: .milliseconds(300))
                let url = URL(fileURLWithPath: path)
                guard let stamp = (try? FileManager.default.attributesOfItem(atPath: path))?[.modificationDate] as? Date,
                      stamp != lastCommandStamp,
                      let text = try? String(contentsOf: url, encoding: .utf8) else { continue }
                lastCommandStamp = stamp
                for line in text.split(separator: "\n").map({ $0.trimmingCharacters(in: .whitespaces) }) where !line.isEmpty {
                    await run(command: line, model)
                }
                executedCommands += 1
                let ack = URL(fileURLWithPath: path + ".ack")
                try? Data("done \(executedCommands)\n".utf8).write(to: ack)
            }
        }
    }

    private static func run(command line: String, _ model: AppModel) async {
        let parts = line.split(separator: " ", maxSplits: 1).map(String.init)
        let verb = parts[0], arg = parts.count > 1 ? parts[1] : ""
        NSLog("DEBUG: command \(line)")
        switch verb {
        case "select": await select(arg, model)
        case "sheet": await sheet(arg, model)
        case "lang": if let lang = AppLanguage(rawValue: arg) { LanguageStore.shared.override(lang) }
        case "appearance": NSApp.appearance = NSAppearance(named: arg == "dark" ? .darkAqua : .aqua)
        case "tech": TechnicalDetailsCard.startExpanded = arg == "on"
        case "scroll": DetailCanvasDebug.startAtBottom = arg == "bottom"
        case "speedtest": await speedTest(arg, model)
        case "health":
            await model.loadDiskHealth(bsdName: arg, force: true)
            switch model.diskHealth(bsdName: arg) {
            case .loaded(let h)?:
                NSLog("DEBUG: health \(arg): \(h.connectionText) | \(h.mediaTypeText) | \(h.smartText) | \(h.model ?? "-") | "
                      + h.technicalRows.map { "\($0.title)=\($0.value)" }.joined(separator: ", "))
            case .failed(let message)?: NSLog("DEBUG: health \(arg) FAILED: \(message)")
            default: break
            }
        case "mount-physical": await mountPhysical(mode: arg.isEmpty ? "readWrite" : arg, model)
        case "popover":
            if arg == "show" { showPopoverPanel(model) } else { panel?.orderOut(nil); panel = nil; model.popoverVisible = false }
        case "window":
            let size = arg.split(separator: "x").compactMap { Double($0) }
            if size.count == 2 { resizeMainWindow(width: size[0], height: size[1]) }
        case "eject":
            if let v = volume(named: arg, model) { await model.eject(v) }
        case "refresh": await model.refresh(force: true)
        case "activate": NSApp.activate()
        case "qrcompat":
            if let v = model.volumes.first(where: { model.portal(for: $0) != nil })?.id {
                await model.setPortalPlainHTTP(v, enabled: arg == "on")
            }
        case "forge": await forge(arg, model)
        case "quit": await quit(model)
        default: NSLog("DEBUG: unknown command \(line)")
        }
    }

    // MARK: Forge (Create & Format, utilities)

    /// `forge <verb> [args]`: drives the Forge screens through ForgeCenter, i.e. the same calls
    /// the buttons make (and the same rules: a refused disk can't be selected, nothing starts
    /// without the confirmation sheet being confirmed). Every job runs in the real core.
    private static func forge(_ line: String, _ model: AppModel) async {
        let forge = model.forge
        let parts = line.split(separator: " ", maxSplits: 1).map(String.init)
        let verb = parts.first ?? "", arg = parts.count > 1 ? parts[1] : ""
        switch verb {
        case "tool":
            if let tool = ForgeCenter.Tool(rawValue: arg) { forge.tool = tool }
        case "disks":
            forge.activate()
            await forge.refreshDisks()
            NSLog("DEBUG: forge disks: \(forge.disks.map { "\($0.bsd)=\(ForgeDiskRules.canSelect($0) ? "allowed" : "refused")" }.joined(separator: " "))")
        case "target":
            // target <bsd> [tool]: waits (a few refreshes) for the disk to be listed.
            let f = arg.split(separator: " ").map(String.init)
            guard let bsd = f.first else { return }
            let tool = f.count > 1 ? (ForgeCenter.Tool(rawValue: f[1]) ?? forge.tool) : forge.tool
            forge.activate()
            for _ in 0..<6 where forge.disk(bsd) == nil {
                await forge.refreshDisks()
                if forge.disk(bsd) == nil { try? await Task.sleep(for: .seconds(1)) }
            }
            guard let disk = forge.disk(bsd) else { NSLog("DEBUG: forge target \(bsd): not listed"); return }
            NSLog("DEBUG: forge target \(bsd) for \(tool.rawValue): selected=\(forge.select(disk, for: tool)) refusal=\(disk.refusal ?? "-")")
        case "target-refused":
            // Tries to select a refused disk, as a click would: it must not become the target.
            guard let disk = forge.disks.first(where: { !ForgeDiskRules.canSelect($0) }) else { NSLog("DEBUG: forge no refused disk listed"); return }
            let before = forge.plan.targetBSD
            let ok = forge.select(disk, for: .create)
            NSLog("DEBUG: forge refused \(disk.bsd) selectable=\(ok) target-unchanged=\(forge.plan.targetBSD == before)")
        case "image":
            await forge.loadImage(URL(fileURLWithPath: arg))
            NSLog("DEBUG: forge inspected \(forge.inspection.map { "\($0.fileName) \($0.format) mode=\($0.recommendedMode.rawValue) hybrid=\($0.iso?.hybrid ?? false)" } ?? "FAILED: \(forge.inspectError ?? "-")")")
        case "clear-image": forge.clearImage()
        case "drawer":
            forge.drawerOpen = arg != "closed"
            forge.showAllSections = arg == "all"
        case "drawer-scroll": forge.drawerScrollTarget = arg.isEmpty ? nil : arg
        case "logs": forge.logDrawerOpen = arg != "closed"
        case "format":
            // format <scheme> <fs> [label]
            let f = arg.split(separator: " ", maxSplits: 2).map(String.init)
            forge.clearImage()
            if let scheme = f.first.flatMap(ForgeScheme.init(rawValue:)) { forge.plan.scheme = scheme }
            if f.count > 1, let fs = ForgeFS(rawValue: f[1]) { forge.plan.primary.setFilesystem(fs) }
            forge.plan.primary.label = f.count > 2 ? f[2] : ""
            forge.plan.addSecond = false
        case "mode": if let mode = ForgeWriteMode(rawValue: arg) { forge.plan.mode = mode }
        case "verify": forge.plan.verify = arg != "off"
        case "eject": forge.plan.eject = arg != "off"
        case "checksum": forge.plan.expectedChecksum = arg
        case "demo-options":
            // Sample values typed in the Windows / Linux / Raspberry Pi sections (UI state only;
            // the Wi-Fi fields are left invalid on purpose to show the inline validation).
            forge.plan.windows.bypassHardwareChecks = true
            forge.plan.windows.localAccount = true
            forge.plan.windows.accountName = "Mario"
            forge.plan.windows.skipPrivacy = true
            forge.plan.windows.locale = L("it-IT", "en-GB")
            forge.plan.windows.timeZone = "Europe/Rome"
            forge.plan.linux.persistence = true
            forge.plan.rpi.enabled = true
            forge.plan.rpi.hostname = "lumenpi"
            forge.plan.rpi.user = "mario"
            forge.plan.rpi.ssh = true
            forge.plan.rpi.wifi = true
            forge.plan.rpi.ssid = "Casa"
            forge.plan.rpi.wifiPassword = "short"
            forge.plan.rpi.country = ""
        case "reset-options":
            let target = forge.plan.targetBSD
            forge.plan = ForgePlan()
            forge.plan.targetBSD = target
        case "prepare":
            let ok = forge.prepareCreate()
            NSLog("DEBUG: forge prepare ok=\(ok) issues=\(forge.createIssues.map(\.message))")
        case "go":
            // The confirmation sheet's checkbox + destructive button.
            guard let c = forge.confirmation else { NSLog("DEBUG: forge go: nothing to confirm"); return }
            forge.confirmationAcknowledged = true
            await forge.confirmPending()
            NSLog("DEBUG: forge confirmed \(c.request.op) on \(c.disk.bsd): job=\(forge.jobs[c.tool].map { "\($0.id)" } ?? "none") error=\(forge.startErrors[c.tool] ?? "-")")
        case "cancel-sheet": forge.confirmation = nil
        case "wait":
            // wait <tool> [seconds]
            let f = arg.split(separator: " ").map(String.init)
            let tool = f.first.flatMap(ForgeCenter.Tool.init(rawValue:)) ?? forge.tool
            let secs = f.count > 1 ? (Double(f[1]) ?? 120) : 120
            guard let job = await forge.waitForJob(tool, timeout: .seconds(secs)) else { NSLog("DEBUG: forge wait \(tool.rawValue): no job"); return }
            let result = (try? job.snapshot?.result.map { try ForgeJSON.encode($0) }) ?? nil
            NSLog("DEBUG: forge job \(job.id) \(job.kind) state=\(job.state.rawValue) phases=\(job.snapshot?.phases.map(\.rawValue).joined(separator: ",") ?? "-") logs=\(job.logs.count) error=\(job.errorMessage ?? "-") result=\(result ?? "null")")
        case "done":
            if let tool = ForgeCenter.Tool(rawValue: arg) { await forge.dismiss(tool) }
        case "hash":
            // hash <path>: SHA-256 + SHA-1 + MD5 of a file
            forge.tool = .hash
            forge.hashDraft.source = .file(arg)
            forge.hashDraft.algorithms = Set(HashAlgorithm.allCases)
            await forge.startHash()
            NSLog("DEBUG: forge hash started job=\(forge.jobs[.hash].map { "\($0.id)" } ?? "none") error=\(forge.startErrors[.hash] ?? "-")")
        case "hash-source": forge.hashDraft.source = .file(arg)
        case "hash-algos":
            forge.hashDraft.algorithms = Set(arg.split(separator: ",").compactMap { HashAlgorithm(rawValue: String($0)) })
        case "hash-compare": forge.hashDraft.compareWith = arg
        case "wipe-method": if let m = WipeMethod(rawValue: arg) { forge.wipeMethod = m }
        case "doctor-check":
            forge.tool = .doctor
            NSLog("DEBUG: forge doctor check started=\(forge.prepareDoctor(.check))")
            try? await Task.sleep(for: .milliseconds(300))
        default: NSLog("DEBUG: unknown forge command \(line)")
        }
    }

    // MARK: Windows

    private static func resizeMainWindow(width: Double, height: Double) {
        for window in NSApp.windows where window.identifier?.rawValue.contains(WindowID.main) == true || window.title == "LumenDisk" {
            var frame = window.frame
            frame.origin.y += frame.height - height
            frame.size = CGSize(width: width, height: height)
            window.setFrame(frame, display: true)
        }
    }

    private static func showPopoverPanel(_ model: AppModel) {
        panel?.orderOut(nil)
        let size = MenuBarPopoverView.size
        let panel = NSPanel(contentRect: NSRect(x: 200, y: 200, width: size.width, height: size.height),
                            styleMask: [.titled, .nonactivatingPanel], backing: .buffered, defer: false)
        panel.title = "Popover"
        panel.contentView = NSHostingView(rootView: MenuBarPopoverView()
            .environment(model)
            .environment(\.locale, LanguageStore.shared.language.locale))
        panel.orderFrontRegardless()
        self.panel = panel
    }

    private static func quit(_ model: AppModel) async {
        // Automated runs leave nothing mounted: Shadow changes made by the test are discarded
        // (the core refuses to eject a volume that still has some).
        for v in model.pendingShadowVolumes { _ = await model.rollbackShadow(v) }
        model.forge.confirmation = nil
        await model.forge.cancelAllAndWait()
        model.activeSheet = nil   // an open sheet would hold up NSApp.terminate
        model.showCreateWizard = false
        model.lockInspection = nil
        try? await Task.sleep(for: .milliseconds(400))
        let failed = await model.ejectAllForQuit()
        NSLog("DEBUG: quitting, eject failures: \(failed)")
        model.isTerminating = true
        NSApp.terminate(nil)
    }
}
#endif
