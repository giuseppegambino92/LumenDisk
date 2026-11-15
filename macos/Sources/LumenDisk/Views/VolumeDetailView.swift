import SwiftUI

struct VolumeDetailView: View {
    @Environment(AppModel.self) private var model
    var volume: VolumeStatus

    @State private var pendingMode: MountMode?
    @State private var showShadowDialog = false
    @State private var showForcedReadOnlyAlert = false
    @State private var showEjectShadowDialog = false
    @State private var showUIDEditor = false
    @State private var ruleSaved = false
    @State private var showPendingFiles = false

    private var busyLabel: String? { model.activity(for: volume.id) }
    private var health: NetworkHealth { NetworkHealth(volume: volume) }
    private var pendingSync: UInt64 { volume.hasSyncQueue ? (volume.remote?.pendingOps ?? 0) : 0 }

    var body: some View {
        DetailCanvas {
            header
            if volume.needsRepair { repairBanner }
            if volume.isRemote { healthCard }
            if volume.totalBytes > 0 { capacityCard }
            DiskHealthCard(volume: volume, bsdName: volume.deviceBSDName)
            if volume.isShadowCommitInterrupted { ShadowCommitInterruptedBanner(volume: volume) }
            if !volume.isRemote { modeCard }
            if volume.mode == .shadow { shadowCard }
            featuresCard
            sharingCard
            TechnicalDetailsCard(rows: technicalRows)
        }
        .safeAreaInset(edge: .bottom, spacing: 0) { actionBar }
        .navigationTitle(volume.displayName)
        .navigationSubtitle(abbreviated(volume.mountpoint))
        .toolbar {
            if !volume.isRemote {
                ToolbarItem(placement: .primaryAction) {
                    Button {
                        _ = model.makeRule(from: volume)
                        ruleSaved = true
                    } label: {
                        Label(model.rule(for: volume) == nil ? L("Crea Regola Automatica", "Create Automount Rule") : L("Regola Salvata", "Rule Saved"),
                              systemImage: model.rule(for: volume) == nil ? "wand.and.sparkles" : "checkmark.seal")
                    }
                    .help(L("Ricorda queste impostazioni per questo volume", "Remember these settings for this volume"))
                }
            }
        }
        .confirmationDialog(shadowDialogTitle, isPresented: $showShadowDialog, titleVisibility: .visible) {
            Button(L("Applica e Cambia Modalità", "Apply and Change Mode")) { change(to: pendingMode, resolving: .commit) }
            Button(L("Scarta e Cambia Modalità", "Discard and Change Mode"), role: .destructive) { change(to: pendingMode, resolving: .rollback) }
            Button(L("Annulla", "Cancel"), role: .cancel) { pendingMode = nil }
        } message: {
            Text(L("Per cambiare modalità il volume verrà rimontato. Scegli cosa fare delle modifiche in Shadow Mode.",
                   "The volume will be remounted to change mode. Choose what to do with the Shadow Mode changes."))
        }
        .confirmationDialog(L("Espellere «\(volume.displayName)» con modifiche in sospeso?", "Eject “\(volume.displayName)” with pending changes?"),
                            isPresented: $showEjectShadowDialog, titleVisibility: .visible) {
            if volume.kind != .imageURL {
                Button(L("Applica ed Espelli", "Apply and Eject")) {
                    Task {
                        if await model.commitShadow(volume) { await model.eject(volume) }
                    }
                }
            }
            if volume.keepsShadowChanges {
                // The core keeps them on this Mac and offers them again on the next mount.
                Button(volume.kind == .imageURL ? L("Scollega e Mantieni le Modifiche", "Disconnect and Keep Changes")
                                                : L("Espelli e Mantieni le Modifiche", "Eject and Keep Changes")) {
                    Task { await model.eject(volume) }
                }
            }
            Button(volume.kind == .imageURL ? L("Scarta e Scollega", "Discard and Disconnect") : L("Scarta ed Espelli", "Discard and Eject"), role: .destructive) {
                Task {
                    if await model.rollbackShadow(volume) { await model.eject(volume) }
                }
            }
            Button(L("Annulla", "Cancel"), role: .cancel) {}
        } message: {
            Text(ejectShadowMessage)
        }
        .alert(L("Il volume è protetto in sola lettura", "The volume is protected as read-only"), isPresented: $showForcedReadOnlyAlert) {
            Button(L("Ripara e Sblocca Scrittura", "Repair and Unlock Writing")) { Task { await model.repair(volume) } }
            Button(L("Annulla", "Cancel"), role: .cancel) {}
        } message: {
            Text(volume.forcedReadOnlyReason ?? L("Il volume non è stato chiuso correttamente.", "The volume was not closed properly."))
        }
        .alert(L("Regola automatica salvata", "Automount rule saved"), isPresented: $ruleSaved) {
            Button(L("Mostra Regole", "Show Rules")) { model.selection = .rules }
            Button("OK", role: .cancel) {}
        } message: {
            Text(L("La prossima volta che colleghi «\(volume.displayName)», LumenDisk userà queste impostazioni.",
                   "Next time you connect “\(volume.displayName)”, LumenDisk will use these settings."))
        }
    }

    // MARK: Header

    private var sourceDescription: String {
        var parts: [String] = []
        if volume.isRemote {
            parts.append(volume.kind.title)
            if let host = volume.remoteHost { parts.append(host) }
        } else if volume.isPhysical {
            parts.append(model.physicalPartition(for: volume).flatMap { model.diskMonitor.disk(containing: $0.bsdName)?.title }
                         ?? L("Disco esterno", "External disk"))
        } else {
            parts.append(volume.sourceFileName)
        }
        if volume.partition > 0 { parts.append(L("Partizione \(volume.partition)", "Partition \(volume.partition)")) }
        return parts.joined(separator: " · ")
    }

    private var header: some View {
        HStack(alignment: .center, spacing: 16) {
            IconBadge(systemImage: volume.symbol, tint: volume.tint, size: 60)
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 8) {
                    Text(volume.displayName)
                        .font(.title2.weight(.semibold))
                        .lineLimit(1)
                    if volume.filesystem != .unknown { FilesystemPill(kind: volume.filesystem) }
                    if volume.isRemote { KindPill(kind: volume.kind) }
                }
                Label {
                    Text(sourceDescription).lineLimit(1).truncationMode(.middle)
                } icon: {
                    Image(systemName: volume.isRemote ? "network" : (volume.isPhysical ? "cable.connector" : "doc"))
                }
                .font(.callout)
                .foregroundStyle(.secondary)
            }
            Spacer(minLength: 8)
            if let busyLabel {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text(busyLabel).font(.callout).foregroundStyle(.secondary)
                }
            } else {
                StatusChip(title: volume.readOnly ? L("Sola Lettura", "Read Only") : volume.mode.shortTitle,
                           systemImage: volume.readOnly ? "lock.fill" : volume.mode.symbol,
                           tint: volume.mode == .shadow ? .ldShadowPending : (volume.readOnly ? .secondary : .green))
            }
        }
        .card()
    }

    private var repairBanner: some View {
        DiagnosticBanner(
            title: volume.forcedReadOnlyReason != nil
                ? L("Scrittura sospesa per sicurezza", "Writing paused for safety")
                : L("Il volume non è stato chiuso correttamente", "The volume was not closed properly"),
            message: [volume.forcedReadOnlyReason, volume.dirty.reason]
                .compactMap { $0 }
                .filter { !$0.isEmpty }
                .joined(separator: " ")
                .nonEmpty ?? L("Il flag dirty o il journal indicano operazioni rimaste in sospeso. LumenDisk può completarle e riattivare la scrittura.",
                               "The dirty flag or the journal show unfinished operations. LumenDisk can complete them and turn writing back on."),
            actionTitle: L("Ripara e Sblocca Scrittura", "Repair and Unlock Writing"),
            isWorking: busyLabel != nil
        ) {
            Task { await model.repair(volume) }
        }
    }

    // MARK: Network health (Feature 5)

    private var healthExplanation: String {
        let remote = volume.remote ?? RemoteStatus()
        if volume.kind == .imageURL {
            let fetched = ByteFormat.string(remote.fetchedBytes)
            if let size = remote.remoteSize {
                return L("Vengono scaricati solo i blocchi che apri: finora \(fetched) di \(ByteFormat.string(size)). Le modifiche restano su questo Mac.",
                         "Only the blocks you open are downloaded: \(fetched) of \(ByteFormat.string(size)) so far. Changes stay on this Mac.")
            }
            return L("Vengono scaricati solo i blocchi che apri: finora \(fetched).", "Only the blocks you open are downloaded: \(fetched) so far.")
        }
        switch health.state {
        case .offline:
            return L("Il server non è raggiungibile. Continua pure a lavorare: le modifiche restano al sicuro su questo Mac e verranno inviate appena torna la connessione.",
                     "The server can’t be reached. Keep working: your changes stay safe on this Mac and are sent as soon as the connection is back.")
        case .syncing:
            return L("Le modifiche vengono inviate al server in background, senza rallentare il Finder.",
                     "Changes are being sent to the server in the background, without slowing Finder down.")
        default:
            if let last = remote.lastSync {
                let when = Date(timeIntervalSince1970: TimeInterval(last)).relativeDescription
                return L("Tutto sincronizzato. Ultima sincronizzazione \(when).", "Everything is in sync. Last synced \(when).")
            }
            return L("Tutto sincronizzato.", "Everything is in sync.")
        }
    }

    private var healthCard: some View {
        let files = volume.remote?.pendingFiles ?? []
        return VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .top, spacing: 14) {
                IconBadge(systemImage: health.state == .offline ? "icloud.slash" : "antenna.radiowaves.left.and.right",
                          tint: health.tint, size: 40)
                VStack(alignment: .leading, spacing: 8) {
                    Text(L("Stato della connessione", "Connection status")).font(.headline)
                    HealthPill(health: health)
                    Text(healthExplanation)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    HStack(spacing: 8) {
                        healthButtons
                    }
                    .padding(.top, 2)
                    if pendingSync > 0 {
                        Label(L("Le modifiche in attesa verranno sincronizzate al prossimo collegamento, anche se scolleghi ora.",
                                "Pending changes will sync next time you connect, even if you disconnect now."),
                              systemImage: "info.circle")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                Spacer(minLength: 0)
            }
            if volume.hasSyncQueue && !files.isEmpty {
                DisclosureGroup(isExpanded: $showPendingFiles) {
                    VStack(spacing: 0) {
                        ForEach(Array(files.prefix(50).enumerated()), id: \.offset) { index, path in
                            if index > 0 { Divider().padding(.leading, 34) }
                            HStack(spacing: 10) {
                                Image(systemName: "doc")
                                    .foregroundStyle(.secondary)
                                    .frame(width: 16)
                                Text((path as NSString).lastPathComponent).lineLimit(1)
                                Text((path as NSString).deletingLastPathComponent)
                                    .foregroundStyle(.secondary)
                                    .lineLimit(1)
                                    .truncationMode(.head)
                                Spacer()
                                Image(systemName: "clock")
                                    .font(.caption)
                                    .foregroundStyle(Color.ldPlum)
                                    .help(L("In attesa di sincronizzazione", "Waiting to sync"))
                            }
                            .font(.callout)
                            .padding(.horizontal, 12)
                            .padding(.vertical, 7)
                        }
                    }
                    .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                    .padding(.top, 8)
                } label: {
                    Text(L10n.plural(pendingSync,
                                     one: ("%d file in attesa", "%d file waiting"),
                                     other: ("%d file in attesa", "%d files waiting")))
                        .font(.callout.weight(.medium))
                }
                .padding(.leading, 54)
            }
        }
        .card()
    }

    @ViewBuilder
    private var healthButtons: some View {
        if volume.hasSyncQueue {
            Button {
                Task { await model.syncNow(volume) }
            } label: {
                Label(L("Sincronizza Ora", "Sync Now"), systemImage: "arrow.triangle.2.circlepath")
            }
            .disabled(busyLabel != nil)
        }
        Button {
            disconnect()
        } label: {
            Label(L("Scollega in Sicurezza", "Disconnect Safely"), systemImage: "eject")
        }
        .disabled(busyLabel != nil)
    }

    private func disconnect() {
        if volume.hasPendingShadowChanges {
            showEjectShadowDialog = true
        } else {
            // Pending offline changes are kept by the core and resume on the next mount.
            Task { await model.eject(volume) }
        }
    }

    // MARK: Capacity

    private var capacityCard: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .firstTextBaseline) {
                Text(ByteFormat.string(volume.freeBytes))
                    .font(.system(size: 28, weight: .semibold, design: .rounded))
                    .monospacedDigit()
                    .contentTransition(.numericText())
                Text(L("liberi di \(ByteFormat.string(volume.totalBytes))", "free of \(ByteFormat.string(volume.totalBytes))"))
                    .font(.system(.title3, design: .rounded))
                    .foregroundStyle(.secondary)
                Spacer()
                if volume.totalBytes > 0 {
                    Text(L("\(Int((Double(volume.usedBytes) / Double(volume.totalBytes) * 100).rounded())) % in uso",
                           "\(Int((Double(volume.usedBytes) / Double(volume.totalBytes) * 100).rounded()))% used"))
                        .font(.system(.callout, design: .rounded).weight(.medium))
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
            CapacityBar(total: volume.totalBytes, used: volume.usedBytes, shadowPending: volume.shadowPendingBytes, height: 12)
            HStack(spacing: 28) {
                LegendItem(color: .ldUsed, title: L("Utilizzato", "Used"), value: ByteFormat.string(volume.usedBytes))
                if volume.mode == .shadow {
                    LegendItem(color: .ldShadowPending, title: L("Shadow in sospeso", "Shadow pending"), value: ByteFormat.delta(volume.shadow?.pendingBytes ?? 0))
                }
                LegendItem(color: Color.primary.opacity(0.15), title: L("Libero", "Free"), value: ByteFormat.string(volume.freeBytes))
                Spacer()
            }
        }
        .card()
    }

    // MARK: Mode

    private var modeCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            CardHeader(title: L("Modalità di Montaggio", "Mount Mode"), subtitle: volume.mode.explanation)
            Picker(L("Modalità di Montaggio", "Mount Mode"), selection: Binding(
                get: { volume.mode },
                set: { requestModeChange($0) }
            )) {
                ForEach(MountMode.allCases) { mode in
                    Text(mode.title).tag(mode)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .controlSize(.large)
            .disabled(busyLabel != nil)
            if volume.forcedReadOnlyReason != nil {
                Label(L("La scrittura è bloccata finché il volume non viene riparato.", "Writing is blocked until the volume is repaired."),
                      systemImage: "lock.trianglebadge.exclamationmark")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
        .card()
    }

    private var ejectShadowMessage: String {
        let count = volume.shadow?.totalChanges ?? 0
        var text = volume.kind == .imageURL
            ? L("Le \(count) modifiche esistono solo su questo Mac: l'immagine remota non viene mai modificata.",
                "The \(count) changes only exist on this Mac: the remote image is never modified.")
            : L("Ci sono \(count) modifiche non ancora scritte sul disco.",
                "There are \(count) changes not yet written to the disk.")
        if volume.keepsShadowChanges {
            text += " " + ShadowWording.keptOnEject
        }
        return text
    }

    private var shadowDialogTitle: String {
        L10n.plural(volume.shadow?.totalChanges ?? 0,
                    one: ("C'è %d modifica in sospeso", "There is %d pending change"),
                    other: ("Ci sono %d modifiche in sospeso", "There are %d pending changes"))
    }

    private func requestModeChange(_ mode: MountMode) {
        guard mode != volume.mode else { return }
        if mode != .readOnly, volume.forcedReadOnlyReason != nil {
            showForcedReadOnlyAlert = true
            return
        }
        pendingMode = mode
        if volume.hasPendingShadowChanges {
            showShadowDialog = true
        } else {
            change(to: mode, resolving: nil)
        }
    }

    private func change(to mode: MountMode?, resolving resolution: AppModel.ShadowResolution?) {
        guard let mode else { return }
        pendingMode = nil
        let snapshot = volume
        Task { await model.changeMode(snapshot, to: mode, resolving: resolution) }
    }

    // MARK: Shadow

    private var shadowCard: some View {
        let shadow = volume.shadow ?? ShadowSummary()
        let subtitle: String
        if volume.kind == .imageURL {
            subtitle = L("Le modifiche restano su questo Mac: il file remoto non viene mai modificato.",
                         "Changes stay on this Mac: the remote file is never modified.")
        } else if shadow.isCommitInterrupted {
            subtitle = ShadowWording.appliedProgress(applied: shadow.commitApplied, total: shadow.commitTotal)
        } else if shadow.hasChanges {
            subtitle = shadow.persistent
                ? L("Nulla è ancora stato scritto sul disco fisico. Le modifiche sono salvate su questo Mac e sopravvivono a un'espulsione o alla chiusura di LumenDisk.",
                    "Nothing has been written to the physical disk yet. The changes are saved on this Mac and survive an eject or quitting LumenDisk.")
                : L("Nulla è ancora stato scritto sul disco fisico.", "Nothing has been written to the physical disk yet.")
        } else {
            subtitle = L("Nessuna modifica in sospeso: il disco è identico all'originale.", "No pending changes: the disk is identical to the original.")
        }
        return VStack(alignment: .leading, spacing: 14) {
            HStack {
                CardHeader(title: L("Modifiche in Shadow Mode", "Shadow Mode Changes"), subtitle: subtitle)
                Spacer()
                Button(L("Rivedi Modifiche…", "Review Changes…")) {
                    model.shadowFocusID = volume.id
                    model.selection = .shadow
                }
                .disabled(!shadow.hasChanges && !shadow.isCommitInterrupted)
            }
            HStack(spacing: 10) {
                ShadowStat(kind: .added, count: shadow.added)
                ShadowStat(kind: .modified, count: shadow.modified)
                ShadowStat(kind: .deleted, count: shadow.deleted)
                VStack(alignment: .leading, spacing: 2) {
                    Text(L("In sospeso", "Pending")).font(.caption).foregroundStyle(.secondary)
                    Text(ByteFormat.delta(shadow.pendingBytes))
                        .font(.system(.title3, design: .rounded).weight(.semibold))
                        .monospacedDigit()
                }
                .padding(12)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            }
            if shadow.restored && !shadow.isCommitInterrupted {
                ShadowRestoredNote()
            }
        }
        .card()
    }

    // MARK: Features

    private var featuresCard: some View {
        VStack(alignment: .leading, spacing: 6) {
            CardHeader(title: L("Funzionalità", "Features"))
                .padding(.bottom, 6)
            FeatureToggleRow(
                systemImage: "shield.lefthalf.filled",
                tint: .accentColor,
                title: L("Ghost Shield Attivo", "Ghost Shield On"),
                subtitle: L("Nessun file .DS_Store o ._* scritto sul disco", "No .DS_Store or ._* files written to the disk"),
                isOn: Binding(get: { volume.ghost.enabled },
                              set: { value in Task { await model.setGhostShield(volume, enabled: value) } })
            ) {
                if volume.ghost.enabled {
                    GhostBadge(count: volume.ghost.blockedCount)
                        .padding(.top, 4)
                }
            }
            if !volume.isRemote {
                Divider().padding(.leading, 42)
                FeatureToggleRow(
                    systemImage: "person.2.badge.gearshape.fill",
                    tint: .indigo,
                    title: L("Mappatura Permessi Linux", "Linux Permission Mapping"),
                    subtitle: uidSubtitle,
                    isOn: Binding(get: { volume.uidMap.enabled },
                                  set: { value in Task { await model.setUIDMap(volume, enabled: value, linuxUID: volume.uidMap.linuxUid, linuxGID: volume.uidMap.linuxGid) } }),
                    disabled: !uidMapApplicable
                ) {
                    if uidMapApplicable {
                        HStack(spacing: 6) {
                            TagBadge(text: "UID \(volume.uidMap.macUid) ↔ \(volume.uidMap.linuxUid)")
                            TagBadge(text: "GID \(volume.uidMap.macGid) ↔ \(volume.uidMap.linuxGid)")
                            Button(L("Personalizza…", "Customise…")) { showUIDEditor = true }
                                .buttonStyle(.link)
                                .font(.callout)
                                .disabled(!volume.uidMap.enabled)
                                .popover(isPresented: $showUIDEditor, arrowEdge: .bottom) {
                                    UIDMapEditor(volume: volume)
                                }
                        }
                        .padding(.top, 4)
                        .opacity(volume.uidMap.enabled ? 1 : 0.5)
                    }
                }
            }
        }
        .card()
    }

    private var uidMapApplicable: Bool {
        switch volume.filesystem {
        case .fat12, .fat16, .fat32, .exfat: false
        default: true
        }
    }

    private var uidSubtitle: String {
        guard uidMapApplicable else {
            return L("Non necessaria: FAT ed exFAT non memorizzano proprietari", "Not needed: FAT and exFAT don’t store owners")
        }
        let auto = volume.uidMap.linuxUid == 1000 && volume.uidMap.linuxGid == 1000
        return "UID \(volume.uidMap.macUid) ↔ \(volume.uidMap.linuxUid) " + (auto ? L("automatico", "automatic") : L("personalizzato", "custom"))
    }

    // MARK: Sharing (Features 3 and 4)

    private var sharingCard: some View {
        let share = model.share(for: volume)
        let portal = model.portal(for: volume)
        return VStack(alignment: .leading, spacing: 6) {
            CardHeader(title: L("Condivisione", "Sharing"))
                .padding(.bottom, 6)
            if volume.kind != .lan {
                FeatureToggleRow(
                    systemImage: "person.2.wave.2.fill",
                    tint: .ldIndigo,
                    title: L("Condividi nella Rete Locale", "Share on Local Network"),
                    subtitle: L("Gli altri Mac con LumenDisk lo vedono nella barra laterale e si collegano con un codice.",
                                "Other Macs running LumenDisk see it in their sidebar and connect with a code."),
                    isOn: Binding(get: { share != nil },
                                  set: { value in Task { await model.setSharing(volume, enabled: value) } }),
                    disabled: busyLabel != nil
                )
                if let share {
                    SharePanel(volume: volume, share: share)
                        .padding(.leading, 42)
                        .padding(.bottom, 6)
                        .transition(.opacity.combined(with: .move(edge: .top)))
                }
                Divider().padding(.leading, 42)
            }
            HStack(alignment: .center, spacing: 12) {
                IconBadge(systemImage: "qrcode", tint: .ldPlum, size: 30)
                VStack(alignment: .leading, spacing: 3) {
                    Text(L("Ricevi/Invia via Wi-Fi (QR)", "Send & Receive over Wi-Fi (QR)")).font(.body.weight(.medium))
                    Text(L("Da iPhone, iPad o Android: inquadra il codice, nessuna app da installare.",
                           "From iPhone, iPad or Android: scan the code, no app to install."))
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    if let portal {
                        Label(L("Attivo · \(portal.uploads) ricevuti · \(portal.downloads) inviati",
                                "On · \(portal.uploads) received · \(portal.downloads) sent"), systemImage: "dot.radiowaves.left.and.right")
                            .font(.caption.weight(.medium))
                            .foregroundStyle(Color.ldPlum)
                            .monospacedDigit()
                            .padding(.top, 2)
                    }
                }
                Spacer(minLength: 12)
                Button(portal == nil ? L("Avvia…", "Start…") : L("Mostra QR…", "Show QR…")) {
                    Task { await model.showPortal(for: volume) }
                }
                .disabled(busyLabel != nil)
            }
            .padding(.vertical, 4)
        }
        .card()
    }

    // MARK: Technical details

    private var technicalRows: [TechnicalRow] {
        var rows: [TechnicalRow] = []
        if volume.isRemote {
            rows.append(TechnicalRow(title: "URL", value: volume.remoteURL))
        } else {
            if let bsd = volume.deviceBSDName {
                rows.append(TechnicalRow(title: L("Nodo dispositivo", "Device node"), value: "/dev/\(bsd)"))
                if let partition = model.physicalPartition(for: volume), !partition.mediaName.isEmpty {
                    rows.append(TechnicalRow(title: L("Supporto", "Media"), value: partition.mediaName))
                }
            }
            rows.append(TechnicalRow(title: L("Origine", "Source"), value: volume.sourcePath))
        }
        if volume.partition > 0 { rows.append(TechnicalRow(title: L("Partizione", "Partition"), value: "\(volume.partition)")) }
        if volume.filesystem != .unknown {
            rows.append(TechnicalRow(title: "File system", value: "\(volume.filesystem.badge) · \(volume.filesystem.familyName)"))
        }
        if !volume.label.isEmpty { rows.append(TechnicalRow(title: L("Etichetta", "Label"), value: volume.label)) }
        if !volume.uuid.isEmpty { rows.append(TechnicalRow(title: "UUID", value: volume.uuid)) }
        rows.append(TechnicalRow(title: L("Punto di montaggio", "Mount point"), value: volume.mountpoint))
        if volume.port > 0 { rows.append(TechnicalRow(title: L("Server NFS locale", "Local NFS server"), value: "127.0.0.1:\(volume.port)")) }
        if let journal = volume.journal, journal.enabled {
            rows.append(TechnicalRow(title: L("Transazioni journal", "Journal transactions"), value: "\(journal.transactions)"))
            if journal.replayedAtMount > 0 {
                rows.append(TechnicalRow(title: L("Riprodotte al montaggio", "Replayed at mount"), value: "\(journal.replayedAtMount)"))
            }
        }
        if let remote = volume.remote {
            rows.append(TechnicalRow(title: L("Cache locale", "Local cache"), value: ByteFormat.string(remote.cacheBytes)))
            rows.append(TechnicalRow(title: L("Scaricati dal server", "Fetched from server"), value: ByteFormat.string(remote.fetchedBytes)))
            if let size = remote.remoteSize { rows.append(TechnicalRow(title: L("Dimensione remota", "Remote size"), value: ByteFormat.string(size))) }
            if let latency = remote.latencyMs { rows.append(TechnicalRow(title: L("Latenza", "Latency"), value: "\(latency) ms")) }
            if volume.hasSyncQueue { rows.append(TechnicalRow(title: L("Operazioni in coda", "Queued operations"), value: "\(remote.pendingOps)")) }
            if let error = remote.lastError, !error.isEmpty { rows.append(TechnicalRow(title: L("Ultimo errore", "Last error"), value: error)) }
        }
        if let share = model.share(for: volume) {
            rows.append(TechnicalRow(title: L("Condivisione", "Share"),
                                     value: "_lumendisk._tcp · " + (share.addresses.isEmpty ? "" : share.addresses.joined(separator: ", ") + " · ") + L("porta \(share.port)", "port \(share.port)")))
            rows.append(TechnicalRow(title: L("Traffico condivisione", "Share traffic"),
                                     value: "↑ \(ByteFormat.string(share.bytesSent))  ↓ \(ByteFormat.string(share.bytesReceived))"))
        }
        if let portal = model.portal(for: volume) {
            rows.append(TechnicalRow(title: L("Portale Wi-Fi", "Wi-Fi portal"), value: portal.url))
        }
        if volume.mountedAt > 0 {
            rows.append(TechnicalRow(title: L("Montato", "Mounted"), value: Date(timeIntervalSince1970: TimeInterval(volume.mountedAt)).relativeDescription))
        }
        if let bsd = volume.deviceBSDName, case .loaded(let health)? = model.diskHealth(bsdName: bsd) {
            rows.append(contentsOf: health.technicalRows)
        }
        if let journal = volume.journal, let wal = journal.walBytes {
            rows.append(TechnicalRow(title: L("Journal (WAL)", "Journal (WAL)"),
                                     value: (journal.mode.map { $0 + " · " } ?? "") + ByteFormat.string(wal)))
        }
        if let share = model.share(for: volume), let fingerprint = share.fingerprint {
            rows.append(TechnicalRow(title: L("Certificato TLS (SHA-256)", "TLS certificate (SHA-256)"), value: fingerprint))
        }
        rows.append(TechnicalRow(title: "ID", value: volume.id))
        return rows.uniquedByTitle()
    }

    // MARK: Actions

    private var actionBar: some View {
        ActionBar {
            Button {
                model.openInFinder(volume)
            } label: {
                Label(L("Apri nel Finder", "Open in Finder"), systemImage: "folder")
            }
            if volume.needsRepair {
                Button {
                    Task { await model.repair(volume) }
                } label: {
                    Label(L("Ripara Flag Dirty", "Repair Dirty Flag"), systemImage: "bandage")
                }
                .disabled(busyLabel != nil)
            }
            Button {
                model.inspectLocks(volume)
            } label: {
                Label(L("Chi Blocca il Disco?", "Who’s Using the Disk?"), systemImage: "lock.rectangle.stack")
            }
            Spacer()
            Button {
                if volume.isRemote {
                    disconnect()
                } else if volume.hasPendingShadowChanges {
                    showEjectShadowDialog = true
                } else {
                    Task { await model.eject(volume) }
                }
            } label: {
                Label(volume.isRemote ? L("Scollega in Sicurezza", "Disconnect Safely") : L("Espelli in Sicurezza", "Eject Safely"),
                      systemImage: "eject.fill")
            }
            .buttonStyle(.borderedProminent)
            .disabled(busyLabel != nil)
        }
        .controlSize(.large)
    }
}

/// Pairing code and client count of a running LAN share.
private struct SharePanel: View {
    var volume: VolumeStatus
    var share: ShareStatus

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .center, spacing: 18) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(L("Codice di abbinamento", "Pairing code"))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    CodeDigits(code: share.pairingCode)
                        .foregroundStyle(.primary)
                }
                Rectangle()
                    .fill(Color.ldBorder)
                    .frame(width: 1, height: 44)
                VStack(alignment: .leading, spacing: 2) {
                    Text(L("Collegati ora", "Connected now"))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    HStack(alignment: .firstTextBaseline, spacing: 5) {
                        Text("\(share.clients)")
                            .font(.system(size: 26, weight: .semibold, design: .rounded))
                            .monospacedDigit()
                            .contentTransition(.numericText(value: Double(share.clients)))
                        Text(L10n.plural(share.clients, one: ("dispositivo", "device"), other: ("dispositivi", "devices")))
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 0)
            }
            HStack(spacing: 10) {
                if share.isEncrypted {
                    Label(L("Connessione cifrata (TLS)", "Encrypted connection (TLS)"), systemImage: "lock.fill")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(Color.ldGood)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 3)
                        .background(Color.ldGood.opacity(0.12), in: Capsule())
                        .help(L("I dati viaggiano cifrati e chi si collega verifica il certificato di questo Mac.",
                                "Data travels encrypted and connecting Macs verify this Mac’s certificate."))
                }
                Label {
                    Text(visibility)
                } icon: {
                    Image(systemName: "dot.radiowaves.left.and.right")
                }
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .animation(.snappy, value: share.clients)
    }

    private var visibility: String {
        let name = share.serviceName.isEmpty ? volume.displayName : share.serviceName
        if let address = share.preferredAddress {
            return L("Visibile come «\(name)» su \(address)", "Visible as “\(name)” at \(address)")
        }
        return L("Visibile come «\(name)»", "Visible as “\(name)”")
    }
}

private struct ShadowStat: View {
    var kind: ShadowChangeKind
    var count: UInt64

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: kind.symbol)
                .font(.system(size: 20))
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(kind.tint)
            VStack(alignment: .leading, spacing: 2) {
                Text(kind.pluralTitle).font(.caption).foregroundStyle(.secondary)
                Text("\(count)")
                    .font(.system(.title3, design: .rounded).weight(.semibold))
                    .monospacedDigit()
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .accessibilityElement(children: .combine)
    }
}

extension ShadowChangeKind {
    var tint: Color {
        switch self {
        case .added: .green
        case .modified: .ldShadowPending
        case .deleted: .red
        }
    }
}

private struct UIDMapEditor: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    var volume: VolumeStatus
    @State private var uid: Int = 1000
    @State private var gid: Int = 1000

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(L("Mappatura Permessi Linux", "Linux Permission Mapping")).font(.headline)
            Text(L("I file di proprietà dell'utente Linux appaiono come tuoi su macOS, e i nuovi file vengono creati con questo proprietario sul disco.",
                   "Files owned by the Linux user appear as yours on macOS, and new files are created on the disk with this owner."))
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 10) {
                GridRow {
                    Text(L("UID Linux", "Linux UID")).foregroundStyle(.secondary)
                    TextField("UID", value: $uid, format: .number.grouping(.never))
                        .font(.system(.body, design: .rounded))
                        .frame(width: 90)
                    Text("↔ \(volume.uidMap.macUid) macOS").font(.system(.callout, design: .rounded)).foregroundStyle(.secondary)
                }
                GridRow {
                    Text(L("GID Linux", "Linux GID")).foregroundStyle(.secondary)
                    TextField("GID", value: $gid, format: .number.grouping(.never))
                        .font(.system(.body, design: .rounded))
                        .frame(width: 90)
                    Text("↔ \(volume.uidMap.macGid) macOS").font(.system(.callout, design: .rounded)).foregroundStyle(.secondary)
                }
            }
            .textFieldStyle(.roundedBorder)
            .monospacedDigit()
            HStack {
                Button(L("Ripristina 1000:1000", "Reset to 1000:1000")) { uid = 1000; gid = 1000 }
                Spacer()
                Button(L("Applica", "Apply")) {
                    let u = UInt32(clamping: max(0, uid)), g = UInt32(clamping: max(0, gid))
                    Task { await model.setUIDMap(volume, enabled: true, linuxUID: u, linuxGID: g) }
                    dismiss()
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(18)
        .frame(width: 360)
        .onAppear {
            uid = Int(volume.uidMap.linuxUid)
            gid = Int(volume.uidMap.linuxGid)
        }
    }
}

extension String {
    var nonEmpty: String? { isEmpty ? nil : self }
}

extension Array where Element == TechnicalRow {
    /// Keeps the first row of each title (rows are identified by their title).
    func uniquedByTitle() -> [TechnicalRow] {
        var seen: Set<String> = []
        return filter { seen.insert($0.title).inserted }
    }
}

/// Plain-language state of a physical partition LumenDisk is not serving yet.
enum PhysicalPartitionStatus {
    static func headline(_ partition: DiskDescription) -> String {
        if partition.isMountedByMacOS {
            return partition.filesystemHint == .ntfs
                ? L("Montato da macOS in sola lettura", "Mounted read-only by macOS")
                : L("Montato da macOS", "Mounted by macOS")
        }
        return L("Non montato da macOS · Pronto per LumenDisk", "Not mounted by macOS · Ready for LumenDisk")
    }

    /// Short form for the sidebar and the menu bar.
    static func compact(_ partition: DiskDescription) -> String {
        let size = ByteFormat.string(partition.mediaSize)
        guard partition.isLumenDiskCandidate else { return "\(size) · " + L("Gestito da macOS", "Handled by macOS") }
        if partition.isMountedByMacOS {
            return "\(size) · " + (partition.filesystemHint == .ntfs ? L("macOS, sola lettura", "macOS, read-only") : L("Montato da macOS", "Mounted by macOS"))
        }
        return "\(size) · " + L("Pronto per LumenDisk", "Ready for LumenDisk")
    }
}

extension DiskDescription {
    @MainActor
    func isDiskImageDevice(in model: AppModel) -> Bool {
        model.diskMonitor.disk(containing: bsdName)?.whole.isDiskImage ?? isDiskImage
    }
}

/// "/Users/x/Volumes/pi" → "~/Volumes/pi".
func abbreviated(_ path: String) -> String {
    (path as NSString).abbreviatingWithTildeInPath
}

// MARK: - Physical partition (not yet mounted by LumenDisk)

struct PhysicalPartitionView: View {
    @Environment(AppModel.self) private var model
    var partition: DiskDescription

    @State private var mode: MountMode = .readWrite
    @State private var ghostShield = true
    @State private var uidMap = true
    @State private var inspection: PartitionInfo?
    @State private var inspectionError: String?
    @State private var isInspecting = false

    private var busyLabel: String? { model.activity(for: partition.bsdName) }
    private var supported: Bool { partition.isLumenDiskCandidate }
    private var kind: FilesystemKind {
        if let inspection { return inspection.filesystem }
        return partition.filesystemHint
    }

    var body: some View {
        DetailCanvas {
            header
            if let dirty = inspection?.dirty, dirty.isDirty {
                DiagnosticBanner(
                    title: L("Il volume non è stato chiuso correttamente", "The volume was not closed properly"),
                    message: dirty.reason ?? L("Il journal contiene operazioni in sospeso. Riparalo prima di montarlo in scrittura.",
                                               "The journal contains unfinished operations. Repair it before mounting it read/write."),
                    actionTitle: L("Ripara e Sblocca Scrittura", "Repair and Unlock Writing"),
                    isWorking: busyLabel != nil
                ) {
                    Task {
                        if await model.repairSource(path: partition.devicePath, partition: 0, activityKey: partition.bsdName) {
                            await analyze()
                        }
                    }
                }
            }
            if let session = model.shadowSession(for: partition) {
                savedSessionCard(session)
            }
            if supported {
                optionsCard
                analysisCard
            } else {
                unsupportedCard
            }
            DiskHealthCard(volume: nil, bsdName: partition.bsdName)
            TechnicalDetailsCard(rows: technicalRows)
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            ActionBar {
                Button {
                    Task {
                        let whole = partition.wholeDiskName
                        try? await model.diskMonitor.unmountWholeNative(bsdName: whole)
                        do { try await model.diskMonitor.eject(bsdName: whole) } catch {
                            model.alert = AppAlert(title: L("Impossibile espellere il disco", "Could not eject the disk"), message: error.localizedDescription)
                        }
                    }
                } label: {
                    Label(L("Espelli Disco", "Eject Disk"), systemImage: "eject")
                }
                Spacer()
            }
            .controlSize(.large)
        }
        .navigationTitle(partition.displayName)
        .navigationSubtitle(model.diskMonitor.disk(containing: partition.bsdName)?.title ?? partition.mediaName)
        .onAppear(perform: applyRule)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 16) {
                IconBadge(systemImage: partition.isDiskImageDevice(in: model) ? "cylinder.split.1x2.fill" : "externaldrive.fill",
                          tint: supported ? kind.tint.opacity(0.75) : .gray, size: 60)
                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 8) {
                        Text(partition.displayName).font(.title2.weight(.semibold)).lineLimit(1)
                        FilesystemPill(kind: kind)
                    }
                    Label {
                        Text((model.diskMonitor.disk(containing: partition.bsdName)?.title ?? partition.mediaName)
                             + " · " + ByteFormat.string(partition.mediaSize))
                            .lineLimit(1)
                    } icon: {
                        Image(systemName: "cable.connector")
                    }
                    .font(.callout)
                    .foregroundStyle(.secondary)
                }
                Spacer()
                if let busyLabel {
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.small)
                        Text(busyLabel).font(.callout).foregroundStyle(.secondary)
                    }
                }
            }
            if supported { statusStrip }
        }
        .card()
    }

    /// "Non montato da macOS · Pronto per LumenDisk" + Monta Ora, or the macOS-mounted variant.
    private var statusStrip: some View {
        let mountedByMac = partition.isMountedByMacOS
        return HStack(spacing: 12) {
            Image(systemName: mountedByMac ? "applelogo" : "checkmark.circle.fill")
                .font(.system(size: 18, weight: .medium))
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(mountedByMac ? Color.secondary : Color.ldGood)
            VStack(alignment: .leading, spacing: 2) {
                Text(PhysicalPartitionStatus.headline(partition))
                    .font(.body.weight(.semibold))
                Text(mountedByMac
                     ? L("LumenDisk lo rilascia da macOS e lo rimonta con scrittura, Ghost Shield e Shadow Mode.",
                         "LumenDisk releases it from macOS and remounts it with writing, Ghost Shield and Shadow Mode.")
                     : L("macOS non sa leggere questo file system: LumenDisk sì, in lettura e scrittura.",
                         "macOS can’t read this file system: LumenDisk can, with full read/write."))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 12)
            mountButton
        }
        .padding(14)
        .background((mountedByMac ? Color.ldWell : Color.ldGood.opacity(0.08)),
                    in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }

    /// Shadow changes saved on this Mac for this disk: resume them or discard them.
    private func savedSessionCard(_ session: ShadowSession) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            CardHeader(title: ShadowWording.promptTitle(for: session),
                       subtitle: L("Sono salvate su questo Mac. Finché non le riprendi in Shadow Mode o le scarti, il disco non può essere montato in lettura/scrittura.",
                                   "They are saved on this Mac. Until you resume them in Shadow Mode or discard them, the disk can’t be mounted read/write."),
                       systemImage: "clock.arrow.circlepath")
            ShadowSessionRow(session: session)
        }
        .card()
    }

    private var mountButton: some View {
        Button {
            Task {
                var options = MountOptions(
                    mode: mode, ghostShield: ghostShield,
                    uidMap: model.rule(for: partition)?.uidMapRequest ?? UIDMapRequest(enabled: uidMap, linuxUid: nil, linuxGid: nil))
                // Saved Shadow changes are restored by a Shadow mount (read/write is refused).
                if let session = model.shadowSession(for: partition), options.mode == .readWrite, model.canResume(session) {
                    options.mode = .shadow
                    await model.mountPhysical(partition, options: options, expectSession: session.key)
                } else {
                    await model.mountPhysical(partition, options: options)
                }
            }
        } label: {
            Label(partition.isMountedByMacOS ? L("Monta in Lettura/Scrittura con LumenDisk", "Mount Read/Write with LumenDisk")
                                             : L("Monta Ora", "Mount Now"),
                  systemImage: "arrow.down.to.line.compact")
        }
        .buttonStyle(CallToActionButtonStyle())
        .disabled(busyLabel != nil)
    }

    private var optionsCard: some View {
        VStack(alignment: .leading, spacing: 14) {
            CardHeader(title: L("Monta con LumenDisk", "Mount with LumenDisk"),
                       subtitle: partition.isMountedByMacOS
                       ? L("macOS ha già montato questo volume con funzioni limitate. LumenDisk lo rilascerà e lo rimonterà con pieno supporto.",
                           "macOS has already mounted this volume with limited features. LumenDisk will release it and remount it with full support.")
                       : (FileManager.default.isWritableFile(atPath: partition.devicePath)
                          ? L("Scegli come montarlo, poi premi «Monta Ora».", "Choose how to mount it, then click “Mount Now”.")
                          : L("Ti verrà chiesta la password di amministratore per accedere al dispositivo.",
                              "You’ll be asked for an administrator password to access the device.")))
            if let rule = model.rule(for: partition) {
                Label(L("Regola automatica «\(rule.displayName)» applicata", "Automount rule “\(rule.displayName)” applied"), systemImage: "wand.and.sparkles")
                    .font(.callout)
                    .foregroundStyle(Color.accentColor)
            }
            Picker(L("Modalità", "Mode"), selection: $mode) {
                ForEach(MountMode.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .controlSize(.large)
            Text(mode.explanation).font(.callout).foregroundStyle(.secondary)
            Divider()
            FeatureToggleRow(systemImage: "shield.lefthalf.filled", tint: .accentColor,
                             title: L("Ghost Shield Attivo", "Ghost Shield On"),
                             subtitle: L("Nessun file .DS_Store o ._* scritto sul disco", "No .DS_Store or ._* files written to the disk"),
                             isOn: $ghostShield)
            if ![.fat12, .fat16, .fat32, .exfat].contains(kind) {
                FeatureToggleRow(systemImage: "person.2.badge.gearshape.fill", tint: .indigo,
                                 title: L("Mappatura Permessi Linux", "Linux Permission Mapping"),
                                 subtitle: L("UID 501 ↔ 1000 automatico", "UID 501 ↔ 1000 automatic"),
                                 isOn: $uidMap)
            }
        }
        .card()
    }

    private var analysisCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                CardHeader(title: L("Diagnosi", "Check-up"),
                           subtitle: L("Controlla il flag dirty e il journal prima di montare.", "Checks the dirty flag and the journal before mounting."))
                Spacer()
                Button {
                    Task { await analyze() }
                } label: {
                    if isInspecting { ProgressView().controlSize(.small) } else { Text(L("Verifica Integrità", "Check Integrity")) }
                }
                .disabled(isInspecting || busyLabel != nil)
            }
            if let inspection {
                Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 8) {
                    InfoRow(title: "File system", value: inspection.filesystem.badge)
                    if let label = inspection.label, !label.isEmpty { InfoRow(title: L("Etichetta", "Label"), value: label) }
                    InfoRow(title: L("Stato", "Status"), value: inspection.dirty?.isDirty == true ? L("Da riparare", "Needs repair") : L("Pulito", "Clean"))
                    if let total = inspection.totalBytes, let free = inspection.freeBytes {
                        InfoRow(title: L("Spazio", "Space"),
                                value: L("\(ByteFormat.string(free)) liberi di \(ByteFormat.string(total))", "\(ByteFormat.string(free)) free of \(ByteFormat.string(total))"))
                    }
                }
            } else if let inspectionError {
                Label(inspectionError, systemImage: "exclamationmark.triangle")
                    .font(.callout).foregroundStyle(.secondary)
            } else {
                Text(L("Nessuna verifica eseguita finora.", "No check run yet."))
                    .font(.callout).foregroundStyle(.tertiary)
            }
        }
        .card()
    }

    private var technicalRows: [TechnicalRow] {
        var rows = [TechnicalRow(title: L("Nodo dispositivo", "Device node"), value: partition.devicePath),
                    TechnicalRow(title: L("Dimensione", "Size"), value: ByteFormat.string(partition.mediaSize))]
        if !partition.mediaName.isEmpty { rows.append(TechnicalRow(title: L("Supporto", "Media"), value: partition.mediaName)) }
        if !partition.mediaContent.isEmpty { rows.append(TechnicalRow(title: L("Tipo partizione", "Partition type"), value: partition.mediaContent)) }
        if let uuid = inspection?.uuid ?? partition.volumeUUID { rows.append(TechnicalRow(title: "UUID", value: uuid)) }
        if let path = partition.volumePath { rows.append(TechnicalRow(title: L("Montato da macOS in", "Mounted by macOS at"), value: path.path)) }
        if let proto = partition.deviceProtocol { rows.append(TechnicalRow(title: L("Protocollo", "Protocol"), value: proto)) }
        if case .loaded(let health)? = model.diskHealth(bsdName: partition.bsdName) {
            rows.append(contentsOf: health.technicalRows)
        }
        return rows.uniquedByTitle()
    }

    private var unsupportedCard: some View {
        VStack(alignment: .leading, spacing: 8) {
            CardHeader(title: L("Gestito da macOS", "Handled by macOS"),
                       subtitle: L("Questo volume usa un file system nativo di macOS: non serve LumenDisk per leggerlo o scriverlo.",
                                   "This volume uses a native macOS file system: LumenDisk isn’t needed to read or write it."),
                       systemImage: "applelogo")
            if let path = partition.volumePath {
                Button(L("Apri nel Finder", "Open in Finder")) {
                    if !NSWorkspace.shared.open(path) { NSWorkspace.shared.activateFileViewerSelecting([path]) }
                }
                .padding(.top, 4)
            }
        }
        .card()
    }

    private func applyRule() {
        if let rule = model.rule(for: partition) {
            mode = rule.mode
            ghostShield = rule.ghostShield
            uidMap = rule.uidMapEnabled
        } else if partition.filesystemHint == .ntfs || partition.filesystemHint == .unknown {
            mode = .shadow   // unknown/Windows volumes: start in the safe mode
        }
        if model.shadowSession(for: partition) != nil, mode == .readWrite {
            mode = .shadow   // saved Shadow changes: a read/write mount would be refused
        }
    }

    private func analyze() async {
        isInspecting = true
        defer { isInspecting = false }
        do {
            let result = try await model.inspect(path: partition.devicePath)
            inspection = result.partitions.first
            inspectionError = nil
        } catch {
            inspectionError = error.localizedDescription
        }
    }
}
