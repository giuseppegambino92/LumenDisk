import SwiftUI

/// Pending Changes Inspector for volumes mounted in Shadow Mode.
struct ShadowStagingView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let volumes = model.shadowVolumes
        let saved = model.savedShadowSessions
        Group {
            if !volumes.isEmpty {
                ShadowStagingContent(volumes: volumes, savedSessions: saved)
            } else if !saved.isEmpty {
                DetailCanvas {
                    PageHeader(title: L("Nessun volume in Shadow Mode", "No volumes in Shadow Mode"),
                               subtitle: L("Le modifiche qui sotto sono conservate su questo Mac e aspettano che tu le riprenda o le scarti.",
                                           "The changes below are kept on this Mac, waiting for you to resume or discard them."),
                               systemImage: "square.stack.3d.forward.dottedline", tint: .ldShadowPending)
                    SavedShadowSessionsCard(sessions: saved)
                }
            } else {
                ContentUnavailableView {
                    Label(L("Nessun volume in Shadow Mode", "No volumes in Shadow Mode"), systemImage: "square.stack.3d.forward.dottedline")
                } description: {
                    Text(L("In Shadow Mode le modifiche finiscono in un livello sicuro e nulla viene scritto sul disco finché non premi «Applica su Disco». Scegli Shadow Mode nella scheda di un volume per provarlo.",
                           "In Shadow Mode changes go to a safe layer and nothing is written to the disk until you click “Apply to Disk”. Choose Shadow Mode on a volume to try it."))
                }
            }
        }
        .navigationTitle("Shadow Staging")
        // Connected / not connected and fingerprints may have moved since the last refresh.
        .task { await model.refreshShadowSessions(prompt: false) }
    }
}

private enum ChangeFilter: String, CaseIterable, Identifiable {
    case all, added, modified, deleted
    var id: String { rawValue }
    var title: String {
        switch self {
        case .all: L("Tutte", "All")
        case .added: L("Aggiunte", "Added")
        case .modified: L("Modificate", "Modified")
        case .deleted: L("Eliminate", "Deleted")
        }
    }
    var kind: ShadowChangeKind? {
        switch self {
        case .all: nil
        case .added: .added
        case .modified: .modified
        case .deleted: .deleted
        }
    }
}

private struct ShadowStagingContent: View {
    @Environment(AppModel.self) private var model
    var volumes: [VolumeStatus]
    var savedSessions: [ShadowSession] = []

    @State private var changes: [ShadowChange] = []
    @State private var loadError: String?
    @State private var isLoading = false
    @State private var filter: ChangeFilter = .all
    @State private var search = ""
    @State private var confirmRollback = false
    @State private var selectedChange: ShadowChange.ID?

    private var volume: VolumeStatus {
        volumes.first { $0.id == model.shadowFocusID }
            ?? volumes.max { ($0.shadow?.totalChanges ?? 0) < ($1.shadow?.totalChanges ?? 0) }
            ?? volumes[0]
    }

    private var visibleChanges: [ShadowChange] {
        changes.filter { change in
            (filter.kind == nil || change.kind == filter.kind)
            && (search.isEmpty || change.path.localizedCaseInsensitiveContains(search))
        }
    }

    /// Reload whenever the counters reported by ld_list_volumes move.
    private var reloadKey: String {
        let s = volume.shadow ?? ShadowSummary()
        return "\(volume.id)|\(s.added)|\(s.modified)|\(s.deleted)|\(s.pendingBytes)|\(s.state)|\(s.commitApplied)"
    }

    var body: some View {
        @Bindable var model = model
        let summary = volume.shadow ?? ShadowSummary()
        let busy = model.activity(for: volume.id)
        let interrupted = volume.isShadowCommitInterrupted

        VStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 16) {
                HStack(alignment: .center) {
                    PageHeader(title: L("Modifiche in Sospeso", "Pending Changes"),
                               subtitle: volume.kind == .imageURL
                               ? L("Le modifiche a «\(volume.displayName)» restano su questo Mac: l'immagine remota non cambia.",
                                   "Changes to “\(volume.displayName)” stay on this Mac: the remote image doesn’t change.")
                               : L("Rivedi cosa cambierà su «\(volume.displayName)» prima di scriverlo sul disco.",
                                   "Review what will change on “\(volume.displayName)” before it is written to the disk."),
                               systemImage: "square.stack.3d.forward.dottedline", tint: .ldShadowPending)
                    Spacer()
                    if volumes.count > 1 {
                        Picker("Volume", selection: Binding(
                            get: { volume.id },
                            set: { model.shadowFocusID = $0 }
                        )) {
                            ForEach(volumes) { v in
                                Text(v.displayName).tag(v.id)
                            }
                        }
                        .frame(maxWidth: 220)
                    }
                }

                HStack(spacing: 10) {
                    SummaryTile(title: ShadowChangeKind.added.pluralTitle, value: "\(summary.added)", symbol: ShadowChangeKind.added.symbol, tint: .green)
                    SummaryTile(title: ShadowChangeKind.modified.pluralTitle, value: "\(summary.modified)", symbol: ShadowChangeKind.modified.symbol, tint: .ldShadowPending)
                    SummaryTile(title: ShadowChangeKind.deleted.pluralTitle, value: "\(summary.deleted)", symbol: ShadowChangeKind.deleted.symbol, tint: .red)
                    SummaryTile(title: L("Variazione spazio", "Space change"), value: ByteFormat.delta(summary.pendingBytes), symbol: "internaldrive", tint: .secondary)
                }

                if volume.isShadowCommitInterrupted {
                    ShadowCommitInterruptedBanner(volume: volume)
                } else if summary.restored {
                    ShadowRestoredNote()
                }

                HStack {
                    Picker(L("Filtro", "Filter"), selection: $filter) {
                        ForEach(ChangeFilter.allCases) { Text($0.title).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .frame(maxWidth: 380)
                    Spacer()
                    TextField(L("Cerca percorso", "Search path"), text: $search)
                        .textFieldStyle(.roundedBorder)
                        .frame(maxWidth: 220)
                    Button {
                        Task { await load() }
                    } label: {
                        Image(systemName: "arrow.clockwise")
                    }
                    .accessibilityLabel(L("Aggiorna elenco modifiche", "Refresh the list of changes"))
                    .help(L("Aggiorna", "Refresh"))
                }
            }
            .padding(.horizontal, 28)
            .padding(.top, 24)
            .padding(.bottom, 14)

            changesTable
                .frame(minHeight: 160, maxHeight: .infinity)
                .padding(.horizontal, 28)
                .padding(.bottom, 16)

            if !savedSessions.isEmpty {
                SavedShadowSessionsCard(sessions: savedSessions)
                    .padding(.horizontal, 28)
                    .padding(.bottom, 16)
            }
        }
        .frame(maxWidth: 980)
        .frame(maxWidth: .infinity)
        .background(Color.ldCanvas)
        .safeAreaInset(edge: .bottom, spacing: 0) {
            ActionBar {
                if let busy {
                    ProgressView().controlSize(.small)
                    Text(busy).foregroundStyle(.secondary)
                } else if interrupted {
                    Label(ShadowWording.appliedProgress(applied: summary.commitApplied, total: summary.commitTotal),
                          systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.orange)
                } else {
                    Label(summary.hasChanges
                          ? L10n.plural(summary.totalChanges, one: ("%d modifica pronta", "%d change ready"), other: ("%d modifiche pronte", "%d changes ready"))
                          : L("Il disco è identico all'originale", "The disk is identical to the original"),
                          systemImage: summary.hasChanges ? "tray.full" : "checkmark.circle")
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button(role: .destructive) {
                    confirmRollback = true
                } label: {
                    Label(interrupted ? L("Abbandona il Resto…", "Abandon the Rest…") : L("Scarta Modifiche (Rollback)", "Discard Changes (Rollback)"),
                          systemImage: "arrow.uturn.backward")
                }
                .disabled(!(summary.hasChanges || interrupted) || busy != nil)
                Button {
                    Task {
                        if await model.commitShadow(volume) { await load() }
                    }
                } label: {
                    Label(interrupted ? L("Riprova Applicazione", "Retry Applying") : L("Applica su Disco (Commit)", "Apply to Disk (Commit)"),
                          systemImage: "checkmark.circle.fill")
                }
                .buttonStyle(.borderedProminent)
                .disabled(!(summary.hasChanges || interrupted) || busy != nil || volume.kind == .imageURL)
                .help(volume.kind == .imageURL ? L("Un'immagine su Internet non può essere modificata: le modifiche restano su questo Mac.",
                                                   "An image on the web can’t be modified: changes stay on this Mac.") : "")
            }
            .controlSize(.large)
        }
        .confirmationDialog(interrupted ? ShadowWording.abandonTitle(name: volume.displayName)
                                        : L("Scartare tutte le modifiche su «\(volume.displayName)»?", "Discard all changes on “\(volume.displayName)”?"),
                            isPresented: $confirmRollback, titleVisibility: .visible) {
            Button(interrupted ? ShadowWording.abandonButton
                               : L10n.plural(summary.totalChanges, one: ("Scarta %d Modifica", "Discard %d Change"), other: ("Scarta %d Modifiche", "Discard %d Changes")),
                   role: .destructive) {
                Task {
                    if await model.rollbackShadow(volume) { await load() }
                }
            }
            Button(L("Annulla", "Cancel"), role: .cancel) {}
        } message: {
            Text(interrupted
                 ? ShadowWording.abandonMessage(applied: summary.commitApplied, total: summary.commitTotal)
                 : L("I file aggiunti verranno rimossi e quelli modificati o eliminati torneranno com'erano. L'operazione non si può annullare.",
                     "Added files will be removed and modified or deleted ones will go back to how they were. This can’t be undone."))
        }
        .task(id: reloadKey) { await load() }
    }

    @ViewBuilder
    private var changesTable: some View {
        if let loadError {
            ContentUnavailableView(L("Impossibile leggere le modifiche", "Couldn’t read the changes"), systemImage: "exclamationmark.triangle",
                                   description: Text(loadError))
                .card()
        } else if changes.isEmpty && !isLoading {
            ContentUnavailableView(L("Nessuna modifica in sospeso", "No pending changes"), systemImage: "checkmark.seal",
                                   description: Text(L("Il volume è identico al disco fisico.", "The volume is identical to the physical disk.")))
                .frame(maxHeight: .infinity)
                .card()
        } else {
            Table(visibleChanges, selection: $selectedChange) {
                TableColumn(L("Elemento", "Item")) { change in
                    HStack(spacing: 8) {
                        Image(systemName: change.kind.symbol)
                            .symbolRenderingMode(.hierarchical)
                            .foregroundStyle(change.kind.tint)
                            .accessibilityLabel(change.kind.title)
                        Image(systemName: change.isDir ? "folder" : "doc")
                            .foregroundStyle(.secondary)
                        Text(change.fileName)
                            .lineLimit(1)
                            .strikethrough(change.kind == .deleted, color: .secondary)
                    }
                }
                .width(min: 170, ideal: 210)
                TableColumn(L("Percorso", "Path")) { change in
                    Text(change.parentPath)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .help(change.path)
                }
                .width(min: 120, ideal: 190)
                TableColumn(L("Dimensione", "Size")) { change in
                    Text(sizeText(change))
                        .font(.system(.body, design: .rounded))
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                }
                .width(min: 100, ideal: 140)
                TableColumn(L("Variazione", "Change")) { change in
                    Text(change.isDir ? "—" : ByteFormat.delta(change.delta))
                        .font(.system(.body, design: .rounded).weight(.medium))
                        .monospacedDigit()
                        .foregroundStyle(change.delta > 0 ? Color.green : (change.delta < 0 ? Color.red : Color.secondary))
                        .frame(maxWidth: .infinity, alignment: .trailing)
                }
                .width(min: 80, ideal: 100)
            }
            .tableStyle(.inset(alternatesRowBackgrounds: true))
            .scrollContentBackground(.hidden)
            .background(Color.ldCard)
            .clipShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 16, style: .continuous).strokeBorder(Color.ldBorder, lineWidth: 1))
            .overlay {
                if isLoading && changes.isEmpty { ProgressView() }
            }
        }
    }

    private func sizeText(_ change: ShadowChange) -> String {
        if change.isDir { return L("Cartella", "Folder") }
        switch change.kind {
        case .added: return ByteFormat.string(change.newSize)
        case .deleted: return ByteFormat.string(change.oldSize)
        case .modified: return "\(ByteFormat.string(change.oldSize)) → \(ByteFormat.string(change.newSize))"
        }
    }

    private func load() async {
        isLoading = true
        defer { isLoading = false }
        do {
            let latest = try await model.shadowChanges(volume)
            if latest != changes { changes = latest }
            loadError = nil
        } catch {
            loadError = error.localizedDescription
        }
    }
}

struct SummaryTile: View {
    var title: String
    var value: String
    var symbol: String
    var tint: Color

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: symbol)
                .font(.system(size: 20))
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(tint)
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.caption).foregroundStyle(.secondary)
                Text(value)
                    .font(.system(.title3, design: .rounded).weight(.semibold))
                    .monospacedDigit()
                    .contentTransition(.numericText())
            }
            Spacer(minLength: 0)
        }
        .card(padding: 14)
        .accessibilityElement(children: .combine)
    }
}
