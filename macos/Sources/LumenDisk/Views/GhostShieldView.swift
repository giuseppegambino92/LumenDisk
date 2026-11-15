import SwiftUI

struct GhostShieldView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            if model.volumes.isEmpty {
                ContentUnavailableView {
                    Label(L("Ghost Shield è pronto", "Ghost Shield is ready"), systemImage: "shield.lefthalf.filled")
                } description: {
                    Text(L("Quando monti un volume, Ghost Shield intercetta .DS_Store, file ._*, .Spotlight-V100, .Trashes e .fseventsd: macOS li vede, ma il disco fisico resta pulito.",
                           "When you mount a volume, Ghost Shield intercepts .DS_Store, ._* files, .Spotlight-V100, .Trashes and .fseventsd: macOS sees them, but the physical disk stays clean."))
                }
            } else {
                GhostShieldContent()
            }
        }
        .navigationTitle("Ghost Shield")
    }
}

private struct GhostShieldContent: View {
    @Environment(AppModel.self) private var model
    @State private var entries: [GhostEntry] = []
    @State private var confirmPurge = false

    private var volume: VolumeStatus {
        model.volumes.first { $0.id == model.ghostFocusID }
            ?? model.volumes.max { $0.ghost.blockedCount < $1.ghost.blockedCount }
            ?? model.volumes[0]
    }

    private var reloadKey: String { "\(volume.id)|\(volume.ghost.blockedCount)" }

    var body: some View {
        @Bindable var model = model
        DetailCanvas {
            hero
            if model.volumes.count > 1 {
                volumeStrip
            }
            volumeCard
            rulesCard
            entriesCard
        }
        .task(id: reloadKey) {
            entries = await model.ghostEntries(volume)
        }
        .confirmationDialog(L("Svuotare il vault di «\(volume.displayName)»?", "Empty the vault of “\(volume.displayName)”?"), isPresented: $confirmPurge, titleVisibility: .visible) {
            Button(L("Svuota Vault", "Empty Vault"), role: .destructive) {
                Task {
                    await model.purgeGhostVault(volume)
                    entries = await model.ghostEntries(volume)
                }
            }
            Button(L("Annulla", "Cancel"), role: .cancel) {}
        } message: {
            Text(L("I file di sistema virtualizzati verranno dimenticati. Non sono mai stati scritti sul disco, quindi nessun tuo documento viene toccato.",
                   "The virtualised system files will be forgotten. They were never written to the disk, so none of your documents is touched."))
        }
    }

    private var hero: some View {
        HStack(spacing: 18) {
            IconBadge(systemImage: "shield.lefthalf.filled", tint: .accentColor, size: 64)
            VStack(alignment: .leading, spacing: 4) {
                Text("\(model.totalGhostBlocked)")
                    .font(.system(size: 34, weight: .semibold, design: .rounded))
                    .monospacedDigit()
                    .contentTransition(.numericText(value: Double(model.totalGhostBlocked)))
                Text(model.totalGhostBlocked == 1
                     ? L("file di sistema macOS bloccato e virtualizzato", "macOS system file blocked and virtualised")
                     : L("file di sistema macOS bloccati e virtualizzati", "macOS system files blocked and virtualised"))
                    .font(.title3)
                Text(L("macOS crede di averli scritti; in realtà restano nel vault di LumenDisk e il disco fisico resta pulito per Linux e Windows. Vale anche per i dischi di rete.",
                       "macOS thinks it wrote them; they actually stay in the LumenDisk vault and the physical disk stays clean for Linux and Windows. Network disks too."))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
        }
        .animation(.snappy, value: model.totalGhostBlocked)
        .card(padding: 22)
    }

    private var volumeStrip: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 10) {
                ForEach(model.volumes) { v in
                    Button {
                        model.ghostFocusID = v.id
                    } label: {
                        HStack(spacing: 8) {
                            Image(systemName: v.symbol)
                                .symbolRenderingMode(.hierarchical)
                                .foregroundStyle(v.tint)
                            Text(v.displayName).lineLimit(1)
                            Text("\(v.ghost.blockedCount)")
                                .font(.system(.caption, design: .rounded).weight(.semibold))
                                .monospacedDigit()
                                .foregroundStyle(.secondary)
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 7)
                        .background(v.id == volume.id ? Color.accentColor.opacity(0.14) : Color.ldCard, in: Capsule())
                        .overlay(Capsule().strokeBorder(v.id == volume.id ? Color.accentColor.opacity(0.35) : Color.ldBorder, lineWidth: 1))
                        .contentShape(Capsule())
                    }
                    .buttonStyle(.plain)
                }
            }
        }
    }

    private var volumeCard: some View {
        HStack(spacing: 14) {
            IconBadge(systemImage: volume.symbol, tint: volume.tint, size: 40)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(volume.displayName).font(.headline)
                    if volume.isRemote { KindPill(kind: volume.kind, compact: true) }
                    if volume.filesystem != .unknown { FilesystemPill(kind: volume.filesystem, compact: true) }
                }
                Text(L10n.plural(volume.ghost.blockedCount,
                                 one: ("%d file intercettato", "%d file intercepted"),
                                 other: ("%d file intercettati", "%d files intercepted"))
                     + " · vault " + ByteFormat.string(volume.ghost.vaultBytes))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
            Spacer()
            Toggle(L("Ghost Shield Attivo", "Ghost Shield On"), isOn: Binding(
                get: { volume.ghost.enabled },
                set: { value in Task { await model.setGhostShield(volume, enabled: value) } }
            ))
            .toggleStyle(.switch)
        }
        .card()
    }

    private var rulesCard: some View {
        let counts = Dictionary(grouping: entries, by: \.category).mapValues(\.count)
        return VStack(alignment: .leading, spacing: 4) {
            CardHeader(title: L("Regole di intercettazione", "Interception rules"),
                       subtitle: L("Scegli quali metadati di macOS tenere lontani dal disco.", "Choose which macOS metadata to keep off the disk."))
                .padding(.bottom, 8)
            ForEach(Array(GhostCategory.allCases.enumerated()), id: \.element) { index, category in
                if index > 0 { Divider().padding(.leading, 42) }
                FeatureToggleRow(
                    systemImage: category.symbol,
                    tint: categoryTint(category),
                    title: category.title,
                    subtitle: category.subtitle,
                    isOn: Binding(
                        get: { volume.ghost.rules[keyPath: category.ruleKeyPath] },
                        set: { value in
                            var rules = volume.ghost.rules
                            rules[keyPath: category.ruleKeyPath] = value
                            Task { await model.setGhostRules(volume, rules: rules) }
                        }),
                    disabled: !volume.ghost.enabled
                ) {
                    if let count = counts[category], count > 0 {
                        Text(L10n.plural(count, one: ("%d intercettato", "%d intercepted"), other: ("%d intercettati", "%d intercepted")))
                            .font(.caption.weight(.medium))
                            .foregroundStyle(.secondary)
                            .monospacedDigit()
                    }
                }
            }
        }
        .opacity(volume.ghost.enabled ? 1 : 0.6)
        .card()
    }

    private func categoryTint(_ category: GhostCategory) -> Color {
        switch category {
        case .dsStore: .blue
        case .appleDouble: .purple
        case .spotlight: .gray
        case .trashes: .pink
        case .fseventsd: .teal
        case .other: .secondary
        }
    }

    private var entriesCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                CardHeader(title: L("File virtualizzati", "Virtualised files"),
                           subtitle: L("Visibili a macOS, mai scritti sul disco.", "Visible to macOS, never written to the disk."))
                Spacer()
                Button(L("Svuota Vault…", "Empty Vault…")) { confirmPurge = true }
                    .disabled(entries.isEmpty)
            }
            if entries.isEmpty {
                HStack(spacing: 10) {
                    Image(systemName: "sparkles")
                        .symbolRenderingMode(.hierarchical)
                        .foregroundStyle(Color.accentColor)
                    Text(L("Ancora nessun file intercettato su questo volume.", "No files intercepted on this volume yet."))
                        .foregroundStyle(.secondary)
                }
                .padding(.vertical, 8)
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(entries.prefix(200).enumerated()), id: \.element.id) { index, entry in
                        if index > 0 { Divider() }
                        HStack(spacing: 10) {
                            Image(systemName: entry.category.symbol)
                                .symbolRenderingMode(.hierarchical)
                                .foregroundStyle(categoryTint(entry.category))
                                .frame(width: 18)
                            Text(entry.path)
                                .font(.callout)
                                .lineLimit(1)
                                .truncationMode(.middle)
                                .textSelection(.enabled)
                            Spacer()
                            Text(ByteFormat.string(entry.size))
                                .font(.system(.callout, design: .rounded))
                                .foregroundStyle(.secondary)
                                .monospacedDigit()
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 7)
                    }
                }
                .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                if entries.count > 200 {
                    Text(L("e altri \(entries.count - 200) file", "and \(entries.count - 200) more files")).font(.caption).foregroundStyle(.secondary)
                }
            }
        }
        .card()
    }
}
