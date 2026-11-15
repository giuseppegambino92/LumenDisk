import AppKit
import SwiftUI

/// Compact menu bar popover (fixed 340×420 so it never clips near the notch or screen edges).
struct MenuBarPopoverView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow

    static let size = CGSize(width: 340, height: 420)

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            list
            Divider()
            footer
        }
        .frame(width: Self.size.width, height: Self.size.height)
        .onAppear {
            model.popoverVisible = true
            model.openMainWindowAction = { [openWindow] in openWindow(id: WindowID.main) }
            Task { await model.refresh(force: true) }
        }
        .onDisappear { model.popoverVisible = false }
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(systemName: "externaldrive.connected.to.line.below")
                .font(.system(size: 15, weight: .medium))
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(Color.accentColor)
            Text("LumenDisk").font(.headline)
            Spacer()
            GhostBadge(count: model.totalGhostBlocked, compact: true)
                .help("Ghost Shield: " + GhostBadge.sentence(model.totalGhostBlocked))
            SettingsMenu(compact: true)
                .menuStyle(.borderlessButton)
                .fixedSize()
                .foregroundStyle(.secondary)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 11)
    }

    private var hasLocal: Bool { !model.localVolumes.isEmpty || !model.unmanagedPartitions.isEmpty }
    private var hasNetwork: Bool { !model.remoteVolumes.isEmpty }

    private var list: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 2) {
                if !hasLocal && !hasNetwork {
                    emptyState
                } else {
                    sectionTitle(L("Dischi Locali", "Local Drives"), systemImage: "externaldrive")
                    if !hasLocal {
                        Text(L("Nessun disco collegato", "No disks connected"))
                            .font(.caption)
                            .foregroundStyle(.tertiary)
                            .padding(.horizontal, 10)
                            .padding(.vertical, 6)
                    }
                    ForEach(model.localVolumes) { volume in
                        MenuVolumeRow(volume: volume, openMain: { openMain { model.selection = .volume(volume.id) } },
                                      showPortal: { openMain { Task { await model.showPortal(for: volume) } } })
                    }
                    ForEach(model.unmanagedPartitions) { partition in
                        MenuPartitionRow(partition: partition, openMain: { openMain { model.selection = .physical(partition.bsdName) } })
                    }
                    sectionTitle(L("Dischi di Rete", "Network Drives"), systemImage: "network")
                    if !hasNetwork {
                        Button {
                            openMain { model.activeSheet = .connectServer }
                        } label: {
                            Label(L("Connetti a Server…", "Connect to Server…"), systemImage: "plus.circle")
                                .font(.caption)
                        }
                        .buttonStyle(.borderless)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 6)
                    }
                    ForEach(model.remoteVolumes) { volume in
                        MenuVolumeRow(volume: volume, openMain: { openMain { model.selection = .volume(volume.id) } },
                                      showPortal: { openMain { Task { await model.showPortal(for: volume) } } })
                    }
                }
            }
            .padding(8)
        }
        .scrollIndicators(.automatic)
    }

    private var emptyState: some View {
        VStack(spacing: 8) {
            Image(systemName: "externaldrive.badge.questionmark")
                .font(.system(size: 30, weight: .light))
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(.secondary)
            Text(L("Nessun disco collegato", "No disks connected")).font(.callout.weight(.medium))
            Text(L("Collega un disco, monta un'immagine o connettiti a un server.", "Connect a disk, mount an image or connect to a server."))
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            HStack(spacing: 8) {
                Button(L("Monta Immagine…", "Mount Image…")) { openMain { ImagePicker.presentOpenPanel(model: model) } }
                Button(L("Connetti a Server…", "Connect to Server…")) { openMain { model.activeSheet = .connectServer } }
            }
            .controlSize(.small)
            .padding(.top, 4)
        }
        .frame(maxWidth: .infinity)
        .padding(.horizontal, 16)
        .padding(.vertical, 56)
    }

    private func sectionTitle(_ text: String, systemImage: String) -> some View {
        Label(text, systemImage: systemImage)
            .font(.caption.weight(.semibold))
            .foregroundStyle(.secondary)
            .padding(.horizontal, 8)
            .padding(.top, 8)
            .padding(.bottom, 2)
    }

    private var footer: some View {
        VStack(spacing: 8) {
            Button {
                Task { await model.ejectAll() }
            } label: {
                HStack {
                    if model.isEjectingAll {
                        ProgressView().controlSize(.small)
                    } else {
                        Image(systemName: "eject.fill")
                    }
                    Text(L("Espelli Tutti in Sicurezza", "Eject All Safely"))
                }
                .frame(maxWidth: .infinity)
            }
            .controlSize(.large)
            .disabled(model.volumes.isEmpty || model.isEjectingAll)

            HStack {
                Button(L("Apri LumenDisk", "Open LumenDisk")) { openMain {} }
                    .buttonStyle(.borderless)
                Spacer()
                Button(L("Esci", "Quit")) { NSApp.terminate(nil) }
                    .buttonStyle(.borderless)
                    .keyboardShortcut("q")
            }
            .font(.callout)
        }
        .padding(12)
    }

    private func openMain(_ then: @escaping @MainActor () -> Void) {
        openWindow(id: WindowID.main)
        NSApp.activate()
        then()
    }
}

private struct MenuVolumeRow: View {
    @Environment(AppModel.self) private var model
    var volume: VolumeStatus
    var openMain: () -> Void
    var showPortal: () -> Void
    @State private var hovering = false

    var body: some View {
        HStack(spacing: 10) {
            IconBadge(systemImage: volume.symbol, tint: volume.tint, size: 30)
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 5) {
                    Text(volume.displayName)
                        .font(.callout.weight(.medium))
                        .lineLimit(1)
                    if volume.isRemote {
                        KindPill(kind: volume.kind, compact: true)
                    } else {
                        FilesystemPill(kind: volume.filesystem, compact: true)
                    }
                    if volume.mode == .shadow {
                        Image(systemName: "square.stack.3d.forward.dottedline")
                            .font(.caption2)
                            .foregroundStyle(Color.ldShadowPending)
                            .help("Shadow Mode")
                    }
                    if volume.needsRepair {
                        Image(systemName: "exclamationmark.triangle.fill")
                            .font(.caption2)
                            .foregroundStyle(.orange)
                            .help(L("Richiede riparazione", "Needs repair"))
                    }
                }
                if volume.totalBytes > 0 {
                    CapacityBar(total: volume.totalBytes, used: volume.usedBytes, shadowPending: volume.shadowPendingBytes, height: 4)
                }
                if volume.isRemote {
                    HealthPill(health: NetworkHealth(volume: volume), compact: true)
                } else {
                    Text(L("\(ByteFormat.string(volume.freeBytes)) liberi", "\(ByteFormat.string(volume.freeBytes)) free"))
                        .font(.system(size: 10.5, design: .rounded))
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
            .contentShape(Rectangle())
            .onTapGesture(perform: openMain)
            Spacer(minLength: 4)
            if let busy = model.activity(for: volume.id) {
                ProgressView().controlSize(.small).help(busy)
            } else {
                iconButton("folder", label: L("Apri nel Finder", "Open in Finder")) { model.openInFinder(volume) }
                iconButton("qrcode", label: L("Ricevi/Invia via Wi-Fi (QR)", "Send & Receive over Wi-Fi (QR)"),
                           active: model.portal(for: volume) != nil, action: showPortal)
                iconButton("eject.fill", label: volume.isRemote ? L("Scollega \(volume.displayName)", "Disconnect \(volume.displayName)")
                                                                : L("Espelli \(volume.displayName)", "Eject \(volume.displayName)")) {
                    // Pending offline changes survive a disconnect; only Shadow changes need a choice.
                    if volume.hasPendingShadowChanges {
                        openMain()   // the detail view asks what to do with pending changes
                    } else {
                        Task { await model.eject(volume) }
                    }
                }
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 7)
        .background(hovering ? Color.primary.opacity(0.06) : .clear, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .onHover { hovering = $0 }
    }

    private func iconButton(_ symbol: String, label: String, active: Bool = false, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 11.5, weight: .medium))
                .frame(width: 25, height: 25)
                .background(active ? Color.ldPlum.opacity(0.16) : Color.primary.opacity(0.06), in: Circle())
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(active ? Color.ldPlum : .secondary)
        .accessibilityLabel(label)
        .help(label)
    }
}

private struct MenuPartitionRow: View {
    @Environment(AppModel.self) private var model
    var partition: DiskDescription
    var openMain: () -> Void
    @State private var hovering = false

    private var supported: Bool { partition.isLumenDiskCandidate }

    var body: some View {
        HStack(spacing: 10) {
            IconBadge(systemImage: "externaldrive", tint: supported ? partition.filesystemHint.tint.opacity(0.7) : .gray, size: 30)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 5) {
                    Text(partition.displayName).font(.callout.weight(.medium)).lineLimit(1)
                    FilesystemPill(kind: partition.filesystemHint, compact: true)
                }
                Text(PhysicalPartitionStatus.compact(partition))
                    .font(.system(size: 10.5))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            .contentShape(Rectangle())
            .onTapGesture(perform: openMain)
            Spacer(minLength: 4)
            if model.isBusy(partition.bsdName) {
                ProgressView().controlSize(.small)
            } else if let session = model.shadowSession(for: partition), model.canResume(session) {
                // Saved Shadow changes: a read/write mount would be refused, resume them instead.
                Button(L("Riprendi", "Resume")) {
                    Task { await model.resumeShadowSession(session) }
                }
                .controlSize(.small)
                .buttonStyle(.borderedProminent)
                .help(ShadowWording.promptTitle(for: session))
                .accessibilityLabel(L("Riprendi le modifiche Shadow su \(partition.displayName)", "Resume the Shadow changes on \(partition.displayName)"))
            } else if supported {
                Button(partition.isMountedByMacOS ? L("Monta", "Mount") : L("Monta Ora", "Mount Now")) {
                    let options = model.rule(for: partition).map(MountOptions.init(rule:)) ?? MountOptions()
                    Task { await model.mountPhysical(partition, options: options, select: false) }
                }
                .controlSize(.small)
                .buttonStyle(.borderedProminent)
                .accessibilityLabel(L("Monta \(partition.displayName) con LumenDisk", "Mount \(partition.displayName) with LumenDisk"))
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 7)
        .background(hovering ? Color.primary.opacity(0.06) : .clear, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .onHover { hovering = $0 }
    }
}
