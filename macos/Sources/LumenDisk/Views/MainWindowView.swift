import SwiftUI

struct MainWindowView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        @Bindable var model = model
        NavigationSplitView {
            SidebarView()
                .navigationSplitViewColumnWidth(min: 240, ideal: 272, max: 340)
        } detail: {
            DetailRouter()
        }
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                SettingsMenu()
            }
        }
        .sheet(item: $model.lockInspection) { inspection in
            LockInspectorSheet(inspection: inspection)
                .environment(model)
        }
        .sheet(isPresented: $model.showCreateWizard) {
            CreateImageSheet()
                .environment(model)
        }
        .sheet(item: $model.activeSheet) { sheet in
            Group {
                switch sheet {
                case .connectServer: ConnectServerSheet()
                case .imageURL: ImageURLSheet()
                case .pair(let peer): PairingSheet(peer: peer)
                case .portal(let id): PortalSheet(volumeID: id)
                }
            }
            .environment(model)
        }
        .alert(model.alert?.title ?? "", isPresented: Binding(
            get: { model.alert != nil },
            set: { if !$0 { model.alert = nil } }
        ), presenting: model.alert) { _ in
            Button("OK", role: .cancel) {}
        } message: { alert in
            Text(alert.message)
        }
        .onAppear {
            model.openMainWindowAction = { [openWindow] in openWindow(id: WindowID.main) }
        }
    }
}

// MARK: - Sidebar

struct SidebarView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        List(selection: $model.selection) {
            Section(L("Dischi Locali", "Local Disks")) {
                if model.localVolumes.isEmpty && model.unmanagedPartitions.isEmpty {
                    Label {
                        Text(L("Nessun disco collegato", "No disks connected")).foregroundStyle(.secondary)
                    } icon: {
                        Image(systemName: "externaldrive.badge.questionmark").foregroundStyle(.tertiary)
                    }
                    .selectionDisabled()
                }
                ForEach(model.localVolumes) { volume in
                    VolumeSidebarRow(volume: volume)
                        .tag(SidebarItem.volume(volume.id))
                }
                ForEach(model.unmanagedPartitions) { partition in
                    PhysicalSidebarRow(partition: partition)
                        .tag(SidebarItem.physical(partition.bsdName))
                }
            }

            Section {
                ForEach(model.remoteVolumes) { volume in
                    VolumeSidebarRow(volume: volume)
                        .tag(SidebarItem.volume(volume.id))
                }
                ForEach(model.lanDiscovery.peers) { peer in
                    PeerSidebarRow(peer: peer)
                        .tag(SidebarItem.lanPeer(peer.id))
                }
                Label {
                    Text(model.remoteVolumes.isEmpty && model.lanDiscovery.peers.isEmpty
                         ? L("Connetti un disco di rete", "Connect a network disk")
                         : L("Rete e Server", "Network & Servers"))
                } icon: {
                    Image(systemName: "network")
                        .symbolRenderingMode(.hierarchical)
                        .foregroundStyle(Color.ldIndigo)
                }
                .tag(SidebarItem.network)
            } header: {
                HStack {
                    Text(L("Dischi di Rete & Remoti", "Network & Remote Disks"))
                    Spacer()
                    Menu {
                        Button(L("Connetti a Server…", "Connect to Server…"), systemImage: "server.rack") {
                            model.activeSheet = .connectServer
                        }
                        Button(L("Monta Immagine da URL…", "Mount Image from URL…"), systemImage: "arrow.down.circle") {
                            model.activeSheet = .imageURL
                        }
                    } label: {
                        Image(systemName: "plus")
                            .font(.system(size: 11, weight: .semibold))
                    }
                    .menuStyle(.borderlessButton)
                    .menuIndicator(.hidden)
                    .fixedSize()
                    .help(L("Aggiungi un disco di rete", "Add a network disk"))
                    .accessibilityLabel(L("Aggiungi un disco di rete", "Add a network disk"))
                }
            }

            Section(L("Protezione", "Protection")) {
                Label("Shadow Staging", systemImage: "square.stack.3d.forward.dottedline")
                    .badge(Int(model.totalShadowChanges))
                    .tag(SidebarItem.shadow)
                Label("Ghost Shield", systemImage: "shield.lefthalf.filled")
                    .tag(SidebarItem.ghost)
            }

            Section(L("Gestione", "Manage")) {
                Label(L("Immagini Disco (.img)", "Disk Images (.img)"), systemImage: "opticaldiscdrive")
                    .tag(SidebarItem.images)
                Label(L("Regole Automatiche", "Automount Rules"), systemImage: "wand.and.sparkles")
                    .badge(model.rules.rules.count)
                    .tag(SidebarItem.rules)
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom, spacing: 0) {
            SidebarFooter()
        }
    }
}

private struct SidebarFooter: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Image(systemName: "shield.lefthalf.filled")
                    .symbolRenderingMode(.hierarchical)
                    .foregroundStyle(Color.accentColor)
                    .font(.system(size: 15))
                VStack(alignment: .leading, spacing: 1) {
                    Text("Ghost Shield")
                        .font(.caption.weight(.semibold))
                    Text(GhostBadge.sentence(model.totalGhostBlocked))
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                        .contentTransition(.numericText(value: Double(model.totalGhostBlocked)))
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            if let error = model.startupError {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption2)
                    .foregroundStyle(.orange)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.primary.opacity(0.04), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .padding(.horizontal, 10)
        .padding(.bottom, 10)
        .animation(.snappy, value: model.totalGhostBlocked)
    }
}

private struct VolumeSidebarRow: View {
    @Environment(AppModel.self) private var model
    var volume: VolumeStatus

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: volume.symbol)
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(volume.tint)
                .font(.system(size: 17))
                .frame(width: 22)
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 5) {
                    Text(volume.displayName).lineLimit(1)
                    if volume.mode == .shadow {
                        Image(systemName: "square.stack.3d.forward.dottedline")
                            .font(.caption2)
                            .foregroundStyle(Color.ldShadowPending)
                            .help("Shadow Mode")
                    } else if volume.mode == .readOnly || volume.readOnly {
                        Image(systemName: "lock.fill")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                            .help(L("Sola Lettura", "Read Only"))
                    }
                    if model.share(for: volume) != nil {
                        Image(systemName: "person.2.wave.2.fill")
                            .font(.caption2)
                            .foregroundStyle(Color.ldIndigo)
                            .help(L("Condiviso nella rete locale", "Shared on your network"))
                    }
                    if volume.needsRepair {
                        Image(systemName: "exclamationmark.triangle.fill")
                            .font(.caption2)
                            .foregroundStyle(.orange)
                            .help(L("Il volume richiede una riparazione", "The volume needs a repair"))
                    }
                    Spacer(minLength: 0)
                    if model.isBusy(volume.id) {
                        ProgressView().controlSize(.mini)
                    }
                }
                if volume.isRemote {
                    let health = NetworkHealth(volume: volume)
                    HStack(spacing: 5) {
                        Circle()
                            .fill(health.tint)
                            .frame(width: 6, height: 6)
                        Text(([health.compactText] + [volume.remoteHost].compactMap { $0 }).joined(separator: " · "))
                            .font(.system(size: 10.5, design: .rounded))
                            .foregroundStyle(.secondary)
                            .monospacedDigit()
                            .lineLimit(1)
                            .truncationMode(.tail)
                    }
                } else {
                    CapacityBar(total: volume.totalBytes, used: volume.usedBytes,
                                shadowPending: volume.shadowPendingBytes, height: 4)
                    Text(L("\(ByteFormat.string(volume.freeBytes)) liberi di \(ByteFormat.string(volume.totalBytes))",
                           "\(ByteFormat.string(volume.freeBytes)) free of \(ByteFormat.string(volume.totalBytes))"))
                        .font(.system(size: 10.5, design: .rounded))
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
        }
        .padding(.vertical, 3)
        .accessibilityElement(children: .combine)
    }
}

private struct PeerSidebarRow: View {
    @Environment(AppModel.self) private var model
    var peer: LANPeer

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "desktopcomputer")
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(Color.teal)
                .font(.system(size: 16))
                .frame(width: 22)
            VStack(alignment: .leading, spacing: 2) {
                Text(peer.name).lineLimit(1)
                Text(peer.filesystem != .unknown ? L("Vicino", "Nearby") + " · " + peer.filesystem.badge : L("Nelle vicinanze", "Nearby"))
                    .font(.system(size: 10.5))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 4)
            Button(L("Connetti", "Connect")) {
                model.activeSheet = .pair(peer)
            }
            .controlSize(.small)
            .buttonStyle(.bordered)
        }
        .padding(.vertical, 3)
    }
}

private struct PhysicalSidebarRow: View {
    @Environment(AppModel.self) private var model
    var partition: DiskDescription

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "externaldrive")
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(partition.isReadyForLumenDisk ? AnyShapeStyle(partition.filesystemHint.tint) : AnyShapeStyle(.secondary))
                .font(.system(size: 17))
                .frame(width: 22)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 5) {
                    Text(partition.displayName).lineLimit(1)
                    Spacer(minLength: 0)
                    if model.isBusy(partition.bsdName) {
                        ProgressView().controlSize(.mini)
                    }
                }
                Text(statusText)
                    .font(.system(size: 10.5))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .padding(.vertical, 3)
        .accessibilityElement(children: .combine)
    }

    private var statusText: String { PhysicalPartitionStatus.compact(partition) }
}

// MARK: - Detail

struct DetailRouter: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            switch model.selection {
            case .volume(let id)?:
                if let volume = model.volume(id: id) {
                    VolumeDetailView(volume: volume)
                        .id(id)
                } else {
                    progressPlaceholder(model.activity(for: id) ?? L("Volume non più disponibile", "Volume no longer available"))
                }
            case .physical(let bsd)?:
                if let partition = model.diskMonitor.partition(bsdName: bsd) {
                    PhysicalPartitionView(partition: partition)
                        .id(bsd)
                } else if let mounted = model.volumes.first(where: { $0.deviceBSDName == bsd }) {
                    VolumeDetailView(volume: mounted)
                } else {
                    ContentUnavailableView(L("Disco scollegato", "Disk disconnected"), systemImage: "externaldrive.badge.xmark",
                                           description: Text(L("Il disco è stato rimosso o espulso.", "The disk was removed or ejected.")))
                }
            case .network?:
                NetworkHubView()
            case .lanPeer(let id)?:
                if let peer = model.lanDiscovery.peer(id: id) {
                    PeerDetailView(peer: peer)
                } else {
                    ContentUnavailableView(L("Mac non più visibile", "Mac no longer visible"), systemImage: "desktopcomputer.trianglebadge.exclamationmark",
                                           description: Text(L("Ha smesso di condividere il disco o ha lasciato la rete.",
                                                               "It stopped sharing the disk or left the network.")))
                }
            case .shadow?:
                ShadowStagingView()
            case .ghost?:
                GhostShieldView()
            case .images?:
                DiskImageMountView()
            case .rules?:
                AutomountRulesView()
            case nil:
                WelcomeView()
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.ldCanvas)
    }

    private func progressPlaceholder(_ text: String) -> some View {
        VStack(spacing: 12) {
            ProgressView()
            Text(text).foregroundStyle(.secondary)
        }
    }
}

struct WelcomeView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        ContentUnavailableView {
            Label(L("Collega un disco", "Connect a disk"), systemImage: "externaldrive.connected.to.line.below")
        } description: {
            Text(L("LumenDisk monta dischi e immagini ext4, NTFS, FAT32 ed exFAT in lettura e scrittura, senza estensioni di sistema.",
                   "LumenDisk mounts ext4, NTFS, FAT32 and exFAT disks and images with full read/write support, without system extensions."))
        } actions: {
            HStack {
                Button(L("Monta Immagine Disco…", "Mount Disk Image…")) { ImagePicker.presentOpenPanel(model: model) }
                    .buttonStyle(.borderedProminent)
                Button(L("Connetti a Server…", "Connect to Server…")) { model.activeSheet = .connectServer }
            }
        }
    }
}
