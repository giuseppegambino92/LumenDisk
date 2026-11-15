import AppKit
import SwiftUI
import UniformTypeIdentifiers

enum ImagePicker {
    static let extensions = ["img", "raw", "iso", "ext4", "ext3", "ext2", "ntfs", "fat", "fat32", "exfat", "bin", "dd", "vfat"]

    static var contentTypes: [UTType] {
        extensions.compactMap { UTType(filenameExtension: $0) }
    }

    static func isSupported(_ url: URL) -> Bool {
        extensions.contains(url.pathExtension.lowercased())
    }

    @MainActor
    static func presentOpenPanel(model: AppModel) {
        let panel = NSOpenPanel()
        panel.title = L("Monta Immagine Disco", "Mount Disk Image")
        panel.message = L("Scegli un'immagine disco ext4, NTFS, FAT32 o exFAT (.img, .raw, .iso, .ext4, .ntfs, .fat, .bin)",
                          "Choose an ext4, NTFS, FAT32 or exFAT disk image (.img, .raw, .iso, .ext4, .ntfs, .fat, .bin)")
        panel.prompt = L("Analizza", "Analyse")
        panel.allowedContentTypes = contentTypes
        panel.allowsMultipleSelection = false
        panel.canChooseDirectories = false
        panel.treatsFilePackagesAsDirectories = false
        NSApp.activate()
        guard panel.runModal() == .OK, let url = panel.url else { return }
        model.openImage(url)
    }
}

struct DiskImageMountView: View {
    @Environment(AppModel.self) private var model
    @State private var isTargeted = false

    var body: some View {
        DetailCanvas {
            PageHeader(title: L("Immagini Disco", "Disk Images"),
                       subtitle: L("Monta immagini di Raspberry Pi, backup e dischi virtuali senza toccare il file originale.",
                                   "Mount Raspberry Pi images, backups and virtual disks without touching the original file."),
                       systemImage: "opticaldiscdrive.fill", tint: .teal)
            if let session = model.imageSession {
                ImageSessionCard(session: session)
                compactDropZone
            } else {
                dropZone
            }
            if !model.recentImages.isEmpty { recentsCard }
        }
        .navigationTitle(L("Immagini Disco", "Disk Images"))
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button {
                    ImagePicker.presentOpenPanel(model: model)
                } label: {
                    Label(L("Monta Immagine Disco…", "Mount Disk Image…"), systemImage: "plus")
                }
                .help(L("Monta Immagine Disco…", "Mount Disk Image…"))
                Button {
                    model.activeSheet = .imageURL
                } label: {
                    Label(L("Monta Immagine da URL…", "Mount Image from URL…"), systemImage: "link")
                }
                .help(L("Monta Immagine da URL…", "Mount Image from URL…"))
                Button {
                    model.showCreateWizard = true
                } label: {
                    Label(L("Crea Disco Virtuale Test…", "Create Test Virtual Disk…"), systemImage: "wand.and.stars")
                }
                .help(L("Crea Disco Virtuale Test…", "Create Test Virtual Disk…"))
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            guard let url = urls.first(where: ImagePicker.isSupported) ?? urls.first else { return false }
            model.openImage(url)
            return true
        } isTargeted: { isTargeted = $0 }
    }

    private var dropZone: some View {
        VStack(spacing: 14) {
            Image(systemName: "arrow.down.doc")
                .font(.system(size: 40, weight: .light))
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(isTargeted ? Color.accentColor : .secondary)
                .symbolEffect(.bounce, value: isTargeted)
            VStack(spacing: 4) {
                Text(L("Trascina qui un'immagine disco", "Drop a disk image here"))
                    .font(.title3.weight(.semibold))
                Text(".img · .raw · .iso · .ext4 · .ntfs · .fat · .fat32 · .bin")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            HStack(spacing: 10) {
                Button(L("Monta Immagine Disco…", "Mount Disk Image…")) { ImagePicker.presentOpenPanel(model: model) }
                    .buttonStyle(.borderedProminent)
                Button(L("Da URL…", "From URL…")) { model.activeSheet = .imageURL }
                Button(L("Crea Disco Virtuale Test…", "Create Test Virtual Disk…")) { model.showCreateWizard = true }
            }
            .controlSize(.large)
            .padding(.top, 4)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 44)
        .background(isTargeted ? Color.accentColor.opacity(0.07) : Color.ldCard,
                    in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .strokeBorder(isTargeted ? Color.accentColor : Color.primary.opacity(0.16),
                              style: StrokeStyle(lineWidth: isTargeted ? 2 : 1.25, dash: [7, 5]))
        )
        .animation(.snappy, value: isTargeted)
    }

    private var compactDropZone: some View {
        HStack(spacing: 10) {
            Image(systemName: "arrow.down.doc")
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(isTargeted ? Color.accentColor : .secondary)
            Text(L("Trascina un'altra immagine qui, oppure", "Drop another image here, or"))
                .foregroundStyle(.secondary)
            Button(L("scegline una…", "choose one…")) { ImagePicker.presentOpenPanel(model: model) }
                .buttonStyle(.link)
            Spacer()
        }
        .font(.callout)
        .padding(14)
        .background(isTargeted ? Color.accentColor.opacity(0.07) : Color.clear,
                    in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous)
            .strokeBorder(isTargeted ? Color.accentColor : Color.primary.opacity(0.14), style: StrokeStyle(lineWidth: 1, dash: [6, 4])))
    }

    private var recentsCard: some View {
        VStack(alignment: .leading, spacing: 10) {
            CardHeader(title: L("Immagini Recenti", "Recent Images"))
            VStack(spacing: 0) {
                ForEach(Array(model.recentImages.enumerated()), id: \.element) { index, path in
                    if index > 0 { Divider().padding(.leading, 40) }
                    RecentImageRow(path: path)
                }
            }
        }
        .card()
    }
}

private struct RecentImageRow: View {
    @Environment(AppModel.self) private var model
    var path: String
    @State private var hovering = false

    var body: some View {
        let url = URL(fileURLWithPath: path)
        let exists = FileManager.default.fileExists(atPath: path)
        let mounted = model.localVolumes.filter { $0.sourcePath == path }
        HStack(spacing: 10) {
            Image(nsImage: NSWorkspace.shared.icon(forFile: path))
                .resizable()
                .frame(width: 28, height: 28)
                .opacity(exists ? 1 : 0.4)
            VStack(alignment: .leading, spacing: 1) {
                Text(url.lastPathComponent).lineLimit(1)
                Text(url.deletingLastPathComponent().path)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.head)
            }
            Spacer()
            if !mounted.isEmpty {
                StatusChip(title: mounted.count == 1 ? L("Montata", "Mounted") : L("\(mounted.count) montate", "\(mounted.count) mounted"), systemImage: "checkmark.circle.fill", tint: .green)
            } else if !exists {
                Text(L("Non trovata", "Not found")).font(.caption).foregroundStyle(.secondary)
            }
            Button(L("Apri", "Open")) { model.openImage(url) }
                .disabled(!exists)
            Button {
                model.forgetRecent(path)
            } label: {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(.tertiary)
            }
            .buttonStyle(.plain)
            .opacity(hovering ? 1 : 0)
            .accessibilityLabel(L("Rimuovi dalle recenti", "Remove from recents"))
            .help(L("Rimuovi dalle recenti", "Remove from recents"))
        }
        .padding(.vertical, 6)
        .onHover { hovering = $0 }
    }
}

private struct ImageSessionCard: View {
    @Environment(AppModel.self) private var model
    var session: ImageSession

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 12) {
                Image(nsImage: NSWorkspace.shared.icon(forFile: session.url.path))
                    .resizable()
                    .frame(width: 40, height: 40)
                VStack(alignment: .leading, spacing: 2) {
                    Text(session.url.lastPathComponent).font(.headline).lineLimit(1)
                    HStack(spacing: 6) {
                        if let inspection = session.inspection {
                            Text(ByteFormat.string(inspection.size))
                                .font(.system(.callout, design: .rounded))
                                .monospacedDigit()
                            Text("·")
                            let scheme = inspection.partitions.first?.scheme ?? "NONE"
                            Text(scheme == "NONE" ? L("Senza tabella partizioni", "No partition table") : L("Tabella \(scheme)", "\(scheme) table"))
                            Text("·")
                            Text(inspection.partitions.count == 1 ? "1 volume" : L("\(inspection.partitions.count) partizioni", "\(inspection.partitions.count) partitions"))
                        } else {
                            Text(session.url.deletingLastPathComponent().path).lineLimit(1).truncationMode(.head)
                        }
                    }
                    .font(.callout)
                    .foregroundStyle(.secondary)
                }
                Spacer()
                Button {
                    NSWorkspace.shared.activateFileViewerSelecting([session.url])
                } label: {
                    Image(systemName: "magnifyingglass")
                }
                .buttonStyle(.borderless)
                .accessibilityLabel(L("Mostra nel Finder", "Show in Finder"))
                .help(L("Mostra nel Finder", "Show in Finder"))
                Button {
                    model.imageSession = nil
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.title3)
                        .foregroundStyle(.tertiary)
                }
                .buttonStyle(.plain)
                .accessibilityLabel(L("Chiudi immagine", "Close image"))
                .help(L("Chiudi", "Close"))
            }

            if session.isInspecting {
                HStack(spacing: 10) {
                    ProgressView().controlSize(.small)
                    Text(L("Analisi delle partizioni…", "Reading the partitions…")).foregroundStyle(.secondary)
                }
                .padding(.vertical, 12)
            } else if let error = session.error {
                HStack(alignment: .top, spacing: 10) {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(L("Impossibile leggere l'immagine", "Couldn’t read the image")).font(.body.weight(.medium))
                        Text(error).font(.callout).foregroundStyle(.secondary)
                    }
                }
                .padding(12)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color.orange.opacity(0.08), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            } else if let inspection = session.inspection {
                if inspection.partitions.isEmpty {
                    Text(L("Nessuna partizione trovata.", "No partitions found.")).foregroundStyle(.secondary)
                } else {
                    VStack(spacing: 0) {
                        ForEach(Array(inspection.partitions.enumerated()), id: \.element.id) { index, partition in
                            if index > 0 { Divider().padding(.leading, 44) }
                            PartitionRow(session: session, partition: partition)
                        }
                    }
                    .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                }

                HStack(spacing: 22) {
                    Toggle(isOn: Binding(get: { session.ghostShield }, set: { model.imageSession?.ghostShield = $0 })) {
                        Label("Ghost Shield", systemImage: "shield.lefthalf.filled")
                    }
                    Toggle(isOn: Binding(get: { session.uidMapEnabled }, set: { model.imageSession?.uidMapEnabled = $0 })) {
                        Label(L("Mappatura Permessi Linux", "Linux Permission Mapping"), systemImage: "person.2.badge.gearshape")
                    }
                    Spacer()
                    Button {
                        Task { await model.mountSelectedImagePartitions() }
                    } label: {
                        if session.isMounting {
                            HStack(spacing: 6) {
                                ProgressView().controlSize(.small)
                                Text(L("Montaggio…", "Mounting…"))
                            }
                        } else {
                            Text(session.selectedCount <= 1 ? L("Monta", "Mount") : L("Monta Selezionate (\(session.selectedCount))", "Mount Selected (\(session.selectedCount))"))
                        }
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
                    .disabled(session.selectedCount == 0 || session.isMounting)
                    .keyboardShortcut(.defaultAction)
                }
                .toggleStyle(.checkbox)
            }
        }
        .card()
    }
}

private struct PartitionRow: View {
    @Environment(AppModel.self) private var model
    var session: ImageSession
    var partition: PartitionInfo

    var body: some View {
        let choice = session.choices[partition.index] ?? .init(selected: false, mode: .readWrite, ruleName: nil)
        let mounted = model.isMounted(path: session.url.path, partition: partition.index)
        HStack(spacing: 12) {
            Toggle(L("Seleziona \(partition.displayName)", "Select \(partition.displayName)"), isOn: Binding(
                get: { choice.selected },
                set: { model.imageSession?.choices[partition.index]?.selected = $0 }
            ))
            .toggleStyle(.checkbox)
            .labelsHidden()
            .disabled(!partition.isMountable || mounted)

            IconBadge(systemImage: partition.isMountable ? "internaldrive.fill" : "questionmark.square.dashed",
                      tint: partition.isMountable ? partition.filesystem.tint : .gray, size: 30)

            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(partition.displayName).font(.body.weight(.medium))
                    if partition.fsType != nil { FilesystemPill(kind: partition.filesystem, compact: true) }
                    if let rule = choice.ruleName {
                        Image(systemName: "wand.and.sparkles")
                            .font(.caption)
                            .foregroundStyle(Color.accentColor)
                            .help(L("Regola automatica «\(rule)»", "Automount rule “\(rule)”"))
                    }
                }
                Text(subtitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                if partition.dirty?.isDirty == true {
                    HStack(spacing: 6) {
                        Label(partition.dirty?.reason ?? L("Da riparare", "Needs repair"), systemImage: "stethoscope")
                            .font(.caption)
                            .foregroundStyle(.orange)
                            .lineLimit(1)
                        Button(session.repairing.contains(partition.index) ? L("Riparazione…", "Repairing…") : L("Ripara", "Repair")) {
                            Task { await model.repairImagePartition(partition) }
                        }
                        .controlSize(.small)
                        .disabled(session.repairing.contains(partition.index))
                    }
                }
            }
            Spacer(minLength: 8)
            Text(ByteFormat.string(partition.size))
                .font(.system(.callout, design: .rounded))
                .foregroundStyle(.secondary)
                .monospacedDigit()
            if mounted {
                StatusChip(title: L("Montata", "Mounted"), systemImage: "checkmark.circle.fill", tint: .green)
            } else if partition.isMountable {
                Picker(L("Modalità", "Mode"), selection: Binding(
                    get: { choice.mode },
                    set: { model.imageSession?.choices[partition.index]?.mode = $0 }
                )) {
                    ForEach(MountMode.allCases) { Label($0.shortTitle, systemImage: $0.symbol).tag($0) }
                }
                .labelsHidden()
                .frame(width: 160)
            } else {
                Text(L("Non supportata", "Not supported")).font(.caption).foregroundStyle(.secondary).frame(width: 160, alignment: .trailing)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .opacity(partition.isMountable ? 1 : 0.65)
    }

    private var subtitle: String {
        var parts: [String] = []
        parts.append(partition.index == 0 ? L("Volume intero", "Whole volume") : L("Partizione \(partition.index)", "Partition \(partition.index)"))
        if !partition.typeName.isEmpty { parts.append(partition.typeName) }
        if let total = partition.totalBytes, let free = partition.freeBytes, total > 0 {
            parts.append(L("\(ByteFormat.string(free)) liberi", "\(ByteFormat.string(free)) free"))
        }
        return parts.joined(separator: " · ")
    }
}

// MARK: - Creation wizard

struct CreateImageSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    @State private var filesystem: ImageFilesystem = .ext4
    @State private var sizeMB: Int = 256
    @State private var label = "LUMENTEST"
    @State private var location: URL?
    @State private var isCreating = false
    @State private var errorMessage: String?
    @State private var createdPath: String?
    @State private var isMounting = false

    private let presets: [Int] = [64, 256, 1024, 4096]

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let createdPath {
                success(path: createdPath)
            } else {
                form
            }
        }
        .frame(width: 500)
    }

    private var form: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(spacing: 14) {
                IconBadge(systemImage: "wand.and.stars", tint: .teal, size: 44)
                VStack(alignment: .leading, spacing: 2) {
                    Text(L("Crea Disco Virtuale Test", "Create Test Virtual Disk")).font(.title3.weight(.semibold))
                    Text(L("Un'immagine vuota e già formattata, perfetta per fare prove in sicurezza.", "An empty, ready-formatted image, perfect for trying things out safely."))
                        .font(.callout).foregroundStyle(.secondary)
                }
            }

            VStack(alignment: .leading, spacing: 8) {
                Text("File system").font(.callout.weight(.medium))
                Picker("File system", selection: $filesystem) {
                    ForEach(ImageFilesystem.allCases) { Text($0.kind.badge).tag($0) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                Text(filesystemHint).font(.caption).foregroundStyle(.secondary)
            }

            VStack(alignment: .leading, spacing: 8) {
                Text(L("Dimensione", "Size")).font(.callout.weight(.medium))
                HStack(spacing: 8) {
                    ForEach(presets, id: \.self) { preset in
                        Button(preset >= 1024 ? "\(preset / 1024) GB" : "\(preset) MB") { sizeMB = preset }
                            .buttonStyle(.bordered)
                            .tint(sizeMB == preset ? .accentColor : nil)
                    }
                    Spacer()
                    TextField("MB", value: $sizeMB, format: .number.grouping(.never))
                        .textFieldStyle(.roundedBorder)
                        .font(.system(.body, design: .rounded))
                        .frame(width: 80)
                        .multilineTextAlignment(.trailing)
                    Text("MB").foregroundStyle(.secondary)
                }
                if sizeMB < Int(filesystem.minimumSizeMB) {
                    Text(L("\(filesystem.kind.badge) richiede almeno \(filesystem.minimumSizeMB) MB.", "\(filesystem.kind.badge) needs at least \(filesystem.minimumSizeMB) MB."))
                        .font(.caption).foregroundStyle(.orange)
                }
            }

            VStack(alignment: .leading, spacing: 8) {
                Text(L("Etichetta", "Label")).font(.callout.weight(.medium))
                TextField(L("Nome del volume", "Volume name"), text: $label)
                    .textFieldStyle(.roundedBorder)
                    .onChange(of: label) { _, new in
                        let limit = filesystem.maxLabelLength
                        if new.count > limit { label = String(new.prefix(limit)) }
                    }
                Text(L("Massimo \(filesystem.maxLabelLength) caratteri per \(filesystem.kind.badge).", "Up to \(filesystem.maxLabelLength) characters for \(filesystem.kind.badge)."))
                    .font(.caption).foregroundStyle(.secondary)
            }

            VStack(alignment: .leading, spacing: 8) {
                Text(L("Posizione", "Location")).font(.callout.weight(.medium))
                HStack {
                    Image(systemName: "folder").foregroundStyle(.secondary)
                    Text(location.map { abbreviated($0.path) } ?? L("Nessuna posizione scelta", "No location chosen"))
                        .lineLimit(1)
                        .truncationMode(.head)
                        .foregroundStyle(location == nil ? .secondary : .primary)
                    Spacer()
                    Button(L("Scegli…", "Choose…"), action: chooseLocation)
                }
                .padding(10)
                .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
            }

            if let errorMessage {
                Label(errorMessage, systemImage: "exclamationmark.triangle.fill")
                    .font(.callout)
                    .foregroundStyle(.orange)
            }

            HStack {
                Spacer()
                Button(L("Annulla", "Cancel"), role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button {
                    Task { await create() }
                } label: {
                    if isCreating {
                        HStack(spacing: 6) { ProgressView().controlSize(.small); Text(L("Creazione…", "Creating…")) }
                    } else {
                        Text(L("Crea Disco", "Create Disk"))
                    }
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
                .disabled(!canCreate || isCreating)
            }
            .controlSize(.large)
        }
        .padding(24)
        .onChange(of: filesystem) { _, fs in
            if sizeMB < Int(fs.minimumSizeMB) { sizeMB = Int(fs.minimumSizeMB) }
            if label.count > fs.maxLabelLength { label = String(label.prefix(fs.maxLabelLength)) }
            if fs == .fat32 { label = label.uppercased() }
            if let location { self.location = location.deletingPathExtension().appendingPathExtension(fs.fileExtension) }
        }
    }

    private var filesystemHint: String {
        switch filesystem {
        case .ext4: L("Il file system di Linux e Raspberry Pi, con journal e permessi.", "The Linux and Raspberry Pi file system, with journal and permissions.")
        case .ntfs: L("Il file system di Windows, adatto a dischi esterni di grandi dimensioni.", "The Windows file system, good for large external disks.")
        case .fat32: L("Massima compatibilità: TV, fotocamere, console. File fino a 4 GB.", "Works everywhere: TVs, cameras, consoles. Files up to 4 GB.")
        case .exfat: L("Compatibile con Windows e macOS, senza limite di 4 GB per file.", "Works with Windows and macOS, with no 4 GB file limit.")
        }
    }

    private var canCreate: Bool {
        location != nil && sizeMB >= Int(filesystem.minimumSizeMB) && !label.trimmingCharacters(in: .whitespaces).isEmpty
    }

    private func chooseLocation() {
        let panel = NSSavePanel()
        panel.title = L("Crea Disco Virtuale Test", "Create Test Virtual Disk")
        panel.prompt = L("Scegli", "Choose")
        panel.nameFieldStringValue = "\(label.isEmpty ? "LumenTest" : label).\(filesystem.fileExtension)"
        panel.canCreateDirectories = true
        panel.isExtensionHidden = false
        if panel.runModal() == .OK, let url = panel.url { location = url }
    }

    private func create() async {
        guard let location else { return }
        isCreating = true
        errorMessage = nil
        defer { isCreating = false }
        let finalLabel = filesystem == .fat32 ? label.uppercased() : label
        do {
            let path = try await model.createImage(CreateImageRequest(
                path: location.path, fs: filesystem, sizeMb: UInt32(clamping: sizeMB), label: finalLabel))
            createdPath = path
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func success(path: String) -> some View {
        VStack(spacing: 16) {
            Image(systemName: "checkmark.circle.fill")
                .font(.system(size: 48))
                .symbolRenderingMode(.palette)
                .foregroundStyle(.white, .green)
            VStack(spacing: 4) {
                Text(L("Disco virtuale creato", "Virtual disk created")).font(.title3.weight(.semibold))
                Text("«\(label)» · \(filesystem.kind.badge) · \(sizeMB >= 1024 ? "\(sizeMB / 1024) GB" : "\(sizeMB) MB")")
                    .foregroundStyle(.secondary)
                Text(abbreviated(path))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .padding(.top, 2)
            }
            HStack {
                Button(L("Mostra nel Finder", "Show in Finder")) {
                    NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
                }
                Spacer()
                Button(L("Fine", "Done")) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button {
                    isMounting = true
                    Task {
                        await model.mountFreshImage(path: path)
                        isMounting = false
                        dismiss()
                    }
                } label: {
                    if isMounting {
                        HStack(spacing: 6) { ProgressView().controlSize(.small); Text(L("Montaggio…", "Mounting…")) }
                    } else {
                        Text(L("Monta Ora", "Mount Now"))
                    }
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
                .disabled(isMounting)
            }
            .controlSize(.large)
            .padding(.top, 8)
        }
        .padding(24)
        .frame(maxWidth: .infinity)
    }
}
