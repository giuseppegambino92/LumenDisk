import AppKit
import SwiftUI
import UniformTypeIdentifiers

// MARK: - Shared styling

extension View {
    /// Translucent "Liquid Glass" panel on macOS 26+, material on earlier systems.
    @ViewBuilder
    func ldGlassPanel(cornerRadius: CGFloat = 16) -> some View {
        if #available(macOS 26.0, *) {
            self.glassEffect(.regular, in: RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
        } else {
            self.background(.regularMaterial, in: RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: cornerRadius, style: .continuous).strokeBorder(Color.ldBorder, lineWidth: 1))
        }
    }
}

enum ForgeImagePicker {
    @MainActor
    static func choose(title: String, prompt: String) -> URL? {
        let panel = NSOpenPanel()
        panel.title = title
        panel.message = L("Immagini .iso, .img, .dmg, .vhd anche compresse (.xz, .gz, .bz2, .zst, .zip): il tipo viene riconosciuto dal contenuto.",
                          ".iso, .img, .dmg, .vhd images, also compressed (.xz, .gz, .bz2, .zst, .zip): the type is recognised from the content.")
        panel.prompt = prompt
        panel.allowedContentTypes = [.data]
        panel.allowsMultipleSelection = false
        panel.canChooseDirectories = false
        panel.treatsFilePackagesAsDirectories = false
        NSApp.activate()
        guard panel.runModal() == .OK else { return nil }
        return panel.url
    }
}

// MARK: - Container

/// "Crea & Formatta / Create & Format": bootable media, formatting and disk utilities.
struct ForgeView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var forge = model.forge
        VStack(spacing: 0) {
            ForgeHeaderBar()
            Divider()
            Group {
                switch forge.tool {
                case .create: ForgeCreateView()
                case .hash: ForgeHashView()
                case .wipe: ForgeWipeView()
                case .doctor: ForgeDoctorView()
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .background(Color.ldCanvas)
        .navigationTitle(L("Crea & Formatta", "Create & Format"))
        .sheet(item: $forge.confirmation) { confirmation in
            ForgeConfirmSheet(confirmation: confirmation)
                .environment(model)
        }
        .onAppear { forge.activate() }
    }
}

private struct ForgeHeaderBar: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        ViewThatFits(in: .horizontal) {
            HStack(alignment: .center, spacing: 14) {
                title
                Spacer(minLength: 12)
                picker
            }
            VStack(alignment: .leading, spacing: 10) {
                title
                picker
            }
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
    }

    private var title: some View {
        HStack(alignment: .center, spacing: 14) {
            IconBadge(systemImage: "wand.and.rays", tint: .ldPlum, size: 38)
            VStack(alignment: .leading, spacing: 2) {
                Text(L("Crea & Formatta", "Create & Format")).font(.title3.weight(.semibold))
                Text(L("Chiavette avviabili, formattazione e strumenti per i dischi", "Bootable drives, formatting and disk tools"))
                    .font(.callout).foregroundStyle(.secondary).lineLimit(1)
            }
            .fixedSize()
        }
    }

    private var picker: some View {
        @Bindable var forge = model.forge
        return Picker(L("Strumento", "Tool"), selection: $forge.tool) {
                ForEach(ForgeCenter.Tool.allCases) { tool in
                    Label {
                        Text(tool.title)
                    } icon: {
                        Image(systemName: forge.jobs[tool]?.isRunning == true ? "circle.dotted" : tool.symbol)
                    }
                    .tag(tool)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            .accessibilityLabel(L("Strumento", "Tool"))
    }
}

// MARK: - Device column

/// The disks the core reports: allowed ones first, refused ones greyed with the reason.
struct ForgeDeviceColumn: View {
    @Environment(AppModel.self) private var model
    var tool: ForgeCenter.Tool
    @State private var showRefused = true

    var body: some View {
        let forge = model.forge
        let allowed = forge.disks.filter(ForgeDiskRules.canSelect)
        let refused = forge.disks.filter { !ForgeDiskRules.canSelect($0) }
        let locked = forge.jobs[tool]?.isRunning == true
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 6) {
                Text(L("Dispositivi", "Devices")).font(.headline)
                Spacer()
                if forge.isLoadingDisks {
                    ProgressView().controlSize(.small)
                        .accessibilityLabel(L("Ricerca dei dischi in corso", "Looking for disks"))
                } else {
                    Button {
                        Task { await forge.refreshDisks() }
                    } label: {
                        Image(systemName: "arrow.clockwise")
                    }
                    .buttonStyle(.borderless)
                    .help(L("Aggiorna l'elenco dei dischi", "Refresh the disk list"))
                    .accessibilityLabel(L("Aggiorna l'elenco dei dischi", "Refresh the disk list"))
                }
            }
            .padding(.horizontal, 14)
            .padding(.top, 14)
            .padding(.bottom, 8)
            ScrollView {
                VStack(alignment: .leading, spacing: 6) {
                    if !forge.disksLoaded && forge.disks.isEmpty {
                        HStack(spacing: 8) {
                            ProgressView().controlSize(.small)
                            Text(L("Ricerca dei dischi…", "Looking for disks…")).font(.callout).foregroundStyle(.secondary)
                        }
                        .padding(.vertical, 10)
                    } else if allowed.isEmpty {
                        VStack(alignment: .leading, spacing: 6) {
                            Label(L("Nessun disco utilizzabile", "No usable disk"), systemImage: "externaldrive.badge.questionmark")
                                .font(.callout.weight(.medium))
                            Text(L("Collega una chiavetta USB o una scheda SD. I dischi interni del Mac non sono mai utilizzabili.",
                                   "Connect a USB stick or an SD card. The Mac's internal disks can never be used."))
                                .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                        }
                        .padding(10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                    }
                    ForEach(allowed) { disk in
                        ForgeDiskRow(disk: disk, selected: forge.selectedBSD(for: tool) == disk.bsd) {
                            forge.select(disk, for: tool)
                        }
                        .disabled(locked)
                    }
                    if !refused.isEmpty {
                        Button {
                            withAnimation(.snappy) { showRefused.toggle() }
                        } label: {
                            HStack(spacing: 4) {
                                Text(L("Non utilizzabili (\(refused.count))", "Not usable (\(refused.count))"))
                                    .font(.caption.weight(.semibold))
                                    .textCase(.uppercase)
                                Image(systemName: "chevron.right")
                                    .font(.caption2.weight(.bold))
                                    .rotationEffect(.degrees(showRefused ? 90 : 0))
                                Spacer()
                            }
                            .foregroundStyle(.secondary)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .padding(.top, 10)
                        .padding(.leading, 4)
                        .accessibilityLabel(L("Dischi non utilizzabili", "Disks that can't be used"))
                        .accessibilityValue(showRefused ? L("Espanso", "Expanded") : L("Compresso", "Collapsed"))
                        if showRefused {
                            ForEach(refused) { disk in
                                ForgeDiskRow(disk: disk, selected: false) {}
                            }
                        }
                    }
                    if let error = forge.disksError {
                        Label(error, systemImage: "exclamationmark.triangle.fill")
                            .font(.caption).foregroundStyle(.orange)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .padding(.horizontal, 10)
                .padding(.bottom, 12)
            }
        }
        .frame(maxHeight: .infinity, alignment: .top)
        .background(Color.ldCard.opacity(0.55))
    }
}

struct ForgeDiskRow: View {
    var disk: ForgeDisk
    var selected: Bool
    var action: () -> Void

    var body: some View {
        let usable = ForgeDiskRules.canSelect(disk)
        Button(action: action) {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: disk.symbol)
                    .font(.system(size: 18))
                    .symbolRenderingMode(.hierarchical)
                    .foregroundStyle(usable ? (selected ? Color.white : Color.accentColor) : Color.secondary)
                    .frame(width: 30, height: 30)
                    .background(usable && selected ? Color.accentColor : Color.ldWell, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                VStack(alignment: .leading, spacing: 2) {
                    Text(disk.title).font(.callout.weight(.semibold)).lineLimit(2)
                    Text([ByteFormat.string(disk.size), disk.busProtocol, disk.bsd].filter { !$0.isEmpty }.joined(separator: " · "))
                        .font(.caption).foregroundStyle(.secondary).lineLimit(1).monospacedDigit()
                    if !disk.volumeNames.isEmpty {
                        Label(disk.volumeNames.joined(separator: ", "), systemImage: "internaldrive")
                            .font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }
                    if let refusal = disk.refusal, !usable {
                        Text(refusal)
                            .font(.caption)
                            .foregroundStyle(.orange)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                Spacer(minLength: 0)
                if selected {
                    Image(systemName: "checkmark.circle.fill").foregroundStyle(Color.accentColor)
                }
            }
            .padding(8)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(selected ? Color.accentColor.opacity(0.12) : Color.clear, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(selected ? Color.accentColor.opacity(0.5) : Color.ldBorder, lineWidth: 1))
            .contentShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
        }
        .buttonStyle(.plain)
        .disabled(!usable)
        .opacity(usable ? 1 : 0.6)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(disk.title), \(ByteFormat.string(disk.size)), \(disk.bsd)")
        .accessibilityValue(usable ? (selected ? L("Selezionato", "Selected") : "") : (disk.refusal ?? L("Non utilizzabile", "Not usable")))
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

/// Device column + the tool's own content, side by side.
struct ForgeToolLayout<Content: View>: View {
    var tool: ForgeCenter.Tool
    @ViewBuilder var content: () -> Content

    var body: some View {
        HStack(spacing: 0) {
            ForgeDeviceColumn(tool: tool).frame(width: 264)
            Divider()
            content().frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

// MARK: - Create & Format

struct ForgeCreateView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let forge = model.forge
        GeometryReader { geo in
            // Wide windows push the drawer beside the content, narrow ones slide it over it.
            let wide = geo.size.width >= 1000
            HStack(spacing: 0) {
                ForgeDeviceColumn(tool: .create).frame(width: 264)
                Divider()
                center(overlayDrawer: forge.drawerOpen && !wide)
                if forge.drawerOpen && wide && forge.jobs[.create] == nil {
                    Divider()
                    ForgeDrawer()
                        .frame(width: ForgeDrawer.width)
                        .background(Color.ldCard.opacity(0.55))
                        .transition(.move(edge: .trailing))
                }
            }
            .animation(.snappy(duration: 0.3), value: forge.drawerOpen)
        }
    }

    @ViewBuilder private func center(overlayDrawer: Bool) -> some View {
        let forge = model.forge
        if let job = forge.jobs[.create] {
            ForgeJobDashboard(job: job)
        } else {
            VStack(spacing: 0) {
                ZStack(alignment: .trailing) {
                    setup
                    if overlayDrawer {
                        ForgeDrawer()
                            .frame(width: ForgeDrawer.width)
                            .ldGlassPanel(cornerRadius: 18)
                            .shadow(color: .black.opacity(0.18), radius: 18, x: -4)
                            .padding(10)
                            .transition(.move(edge: .trailing).combined(with: .opacity))
                    }
                }
                ForgeCreateActionBar()
            }
        }
    }

    private var setup: some View {
        let forge = model.forge
        return ScrollView {
                    VStack(alignment: .leading, spacing: 16) {
                        ForgeDropZone()
                        if forge.inspection == nil {
                            ForgeFormatOnlyCard()
                        }
                        if let error = forge.startErrors[.create] {
                            ForgeErrorBanner(message: error)
                        }
                    }
                    .padding(22)
                    .frame(maxWidth: 760)
                    .frame(maxWidth: .infinity)
                }
    }
}

struct ForgeErrorBanner: View {
    var message: String

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "exclamationmark.octagon.fill").foregroundStyle(.red)
            Text(message).font(.callout).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
        }
        .padding(12)
        .background(Color.red.opacity(0.08), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(Color.red.opacity(0.2), lineWidth: 1))
        .accessibilityElement(children: .combine)
    }
}

/// Drop zone + what was detected in the image.
private struct ForgeDropZone: View {
    @Environment(AppModel.self) private var model
    @State private var isTargeted = false

    var body: some View {
        let forge = model.forge
        VStack(alignment: .leading, spacing: 12) {
            if let inspection = forge.inspection, forge.inspectingPath == nil {
                ForgeDetectionCard(inspection: inspection)
                compact
            } else {
                large
            }
            if let error = forge.inspectError {
                ForgeErrorBanner(message: error)
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            guard let url = urls.first else { return false }
            Task { await forge.loadImage(url) }
            return true
        } isTargeted: { isTargeted = $0 }
    }

    private func choose() {
        if let url = ForgeImagePicker.choose(title: L("Scegli un'immagine", "Choose an image"), prompt: L("Scegli", "Choose")) {
            Task { await model.forge.loadImage(url) }
        }
    }

    private var large: some View {
        let inspecting = model.forge.inspectingPath
        return VStack(spacing: 14) {
            if let inspecting {
                ProgressView().controlSize(.large)
                Text(L("Analisi di \((inspecting as NSString).lastPathComponent)…", "Analysing \((inspecting as NSString).lastPathComponent)…"))
                    .font(.headline)
            } else {
                Image(systemName: "arrow.down.doc")
                    .font(.system(size: 40, weight: .light))
                    .symbolRenderingMode(.hierarchical)
                    .foregroundStyle(isTargeted ? Color.accentColor : .secondary)
                    .symbolEffect(.bounce, value: isTargeted)
                VStack(spacing: 4) {
                    Text(L("Trascina qui un'immagine da scrivere", "Drop an image to write here"))
                        .font(.title3.weight(.semibold))
                    Text(".iso · .img · .dmg · .vhd · .xz · .gz · .bz2 · .zst · .zip")
                        .font(.callout).foregroundStyle(.secondary)
                    Text(L("Oppure formatta il disco senza immagine qui sotto.", "Or format the disk without an image below."))
                        .font(.caption).foregroundStyle(.tertiary)
                }
                Button(L("Scegli…", "Choose…")) { choose() }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
                    .accessibilityLabel(L("Scegli un'immagine", "Choose an image"))
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 36)
        .background(isTargeted ? Color.accentColor.opacity(0.07) : Color.ldCard, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 16, style: .continuous)
            .strokeBorder(isTargeted ? Color.accentColor : Color.primary.opacity(0.16),
                          style: StrokeStyle(lineWidth: isTargeted ? 2 : 1.25, dash: [7, 5])))
        .animation(.snappy, value: isTargeted)
    }

    private var compact: some View {
        HStack(spacing: 10) {
            Image(systemName: "arrow.down.doc").symbolRenderingMode(.hierarchical)
                .foregroundStyle(isTargeted ? Color.accentColor : .secondary)
            Text(L("Trascina un'altra immagine, oppure", "Drop another image, or")).foregroundStyle(.secondary)
            Button(L("scegline una…", "choose one…")) { choose() }.buttonStyle(.link)
            Spacer()
            Button(L("Solo formattazione", "Format only")) { model.forge.clearImage() }
                .help(L("Rimuovi l'immagine e formatta soltanto", "Remove the image and just format"))
        }
        .font(.callout)
        .padding(12)
        .background(isTargeted ? Color.accentColor.opacity(0.07) : Color.clear, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous)
            .strokeBorder(isTargeted ? Color.accentColor : Color.primary.opacity(0.14), style: StrokeStyle(lineWidth: 1, dash: [6, 4])))
    }
}

/// What `ld_forge_inspect` found in the image.
struct ForgeDetectionCard: View {
    @Environment(AppModel.self) private var model
    var inspection: ImageInspection

    private var headline: (title: String, symbol: String, tint: Color) {
        if let w = inspection.windows {
            return ([w.version, w.arch].filter { !$0.isEmpty }.joined(separator: " "), "pc", .ldIndigo)
        }
        if let l = inspection.linux { return (l.distro, "terminal", .orange) }
        if let r = inspection.rpi { return (r.os, "cpu", .pink) }
        if let iso = inspection.iso { return (iso.label.isEmpty ? inspection.fileName : iso.label, "opticaldisc", .teal) }
        return (inspection.fileName, "doc.zipper", .ldSlate)
    }

    var body: some View {
        let h = headline
        let mode = model.forge.plan.effectiveMode(inspection)
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .center, spacing: 12) {
                IconBadge(systemImage: h.symbol, tint: h.tint, size: 42)
                VStack(alignment: .leading, spacing: 2) {
                    Text(h.title).font(.title3.weight(.semibold)).lineLimit(1)
                    Text([inspection.fileName, sizeText].joined(separator: " · "))
                        .font(.callout).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                }
                Spacer(minLength: 0)
            }
            FlowChips(chips: chips)
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: "lightbulb").foregroundStyle(Color.ldAmber)
                VStack(alignment: .leading, spacing: 3) {
                    Text(L("Modalità consigliata: \(inspection.recommendedMode.title)", "Recommended mode: \(inspection.recommendedMode.title)"))
                        .font(.callout.weight(.semibold))
                    if !inspection.explanation.isEmpty {
                        Text(inspection.explanation).font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    }
                    if mode != inspection.recommendedMode {
                        Text(L("Hai scelto la modalità \(mode.title).", "You chose \(mode.title) mode."))
                            .font(.caption).foregroundStyle(.orange)
                    }
                }
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            if let w = inspection.windows { windowsDetails(w) }
            if !inspection.partitions.isEmpty { partitionsList }
            if let sums = inspection.checksumFile {
                Label(L("\((sums.path as NSString).lastPathComponent) contiene il checksum \(sums.algorithm.uppercased()) di questa immagine: verrà controllato prima di scrivere.",
                        "\((sums.path as NSString).lastPathComponent) lists this image's \(sums.algorithm.uppercased()) checksum: it will be checked before writing."),
                      systemImage: "checkmark.seal")
                    .font(.callout).foregroundStyle(Color.ldGood).fixedSize(horizontal: false, vertical: true)
            }
            ForEach(inspection.notes, id: \.self) { note in
                Label(note, systemImage: "info.circle").font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
        }
        .card()
        .accessibilityElement(children: .contain)
    }

    private var sizeText: String {
        if let size = inspection.size { return ByteFormat.string(size) }
        if let est = inspection.sizeEstimate { return "≈ " + ByteFormat.string(est) }
        return ByteFormat.string(inspection.fileSize)
    }

    private var chips: [(String, String)] {
        var out: [(String, String)] = [(inspection.format.uppercased(), "doc")]
        if inspection.compressed { out.append((L("Compressa", "Compressed"), "archivebox")) }
        if let iso = inspection.iso {
            out.append((iso.hybrid ? L("ISO ibrida", "Hybrid ISO") : "ISO 9660", "opticaldisc"))
            if iso.efiBoot { out.append(("UEFI", "bolt.shield")) }
            if iso.elTorito { out.append(("El Torito", "power")) }
            if iso.udf { out.append(("UDF", "doc.text")) }
        }
        if let scheme = inspection.partitionScheme { out.append((scheme, "rectangle.split.3x1")) }
        if let w = inspection.windows {
            if w.biosBoot { out.append(("BIOS", "power")) }
            if !w.uefiBoot.isEmpty { out.append(("UEFI " + w.uefiBoot.joined(separator: "/"), "bolt.shield")) }
        }
        return out
    }

    private func windowsDetails(_ w: WindowsInfo) -> some View {
        Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 6) {
            if let build = w.build { InfoRow(title: L("Build", "Build"), value: "\(build)") }
            if !w.editions.isEmpty { InfoRow(title: L("Edizioni", "Editions"), value: w.editions.joined(separator: ", ")) }
            if !w.languages.isEmpty { InfoRow(title: L("Lingue", "Languages"), value: w.languages.joined(separator: ", ")) }
            if !w.installImage.isEmpty { InfoRow(title: L("Immagine", "Install image"), value: "\(w.installImage) · \(ByteFormat.string(w.installImageSize))") }
            if w.needsSplit {
                InfoRow(title: "FAT32", value: w.splitBlocker ?? L("install.wim supera 4 GB: verrà diviso", "install.wim is over 4 GB: it will be split"))
            }
        }
    }

    private var partitionsList: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(L("Partizioni nell'immagine", "Partitions in the image")).font(.caption.weight(.semibold)).foregroundStyle(.secondary)
            ForEach(inspection.partitions.prefix(6), id: \.self) { p in
                HStack(spacing: 8) {
                    TagBadge(text: "\(p.index)")
                    Text(p.typeName.isEmpty || p.typeName == "Unknown" ? p.scheme : p.typeName).font(.callout).lineLimit(1)
                    if !p.name.isEmpty { Text(p.name).font(.callout).foregroundStyle(.secondary).lineLimit(1) }
                    Spacer()
                    Text(ByteFormat.string(p.size)).font(.callout).foregroundStyle(.secondary).monospacedDigit()
                }
            }
        }
    }
}

/// Wrapping row of small chips.
struct FlowChips: View {
    var chips: [(String, String)]

    var body: some View {
        ChipFlowLayout(spacing: 6) {
            ForEach(Array(chips.enumerated()), id: \.offset) { _, chip in
                StatusChip(title: chip.0, systemImage: chip.1, tint: .secondary)
            }
        }
    }
}

struct ChipFlowLayout: Layout {
    var spacing: CGFloat = 6

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let width = proposal.width ?? 600
        var x: CGFloat = 0, y: CGFloat = 0, row: CGFloat = 0
        for s in subviews {
            let size = s.sizeThatFits(.unspecified)
            if x > 0 && x + size.width > width { x = 0; y += row + spacing; row = 0 }
            x += size.width + spacing
            row = max(row, size.height)
        }
        return CGSize(width: width, height: y + row)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var x = bounds.minX, y = bounds.minY, row: CGFloat = 0
        for s in subviews {
            let size = s.sizeThatFits(.unspecified)
            if x > bounds.minX && x + size.width > bounds.maxX { x = bounds.minX; y += row + spacing; row = 0 }
            s.place(at: CGPoint(x: x, y: y), proposal: ProposedViewSize(size))
            x += size.width + spacing
            row = max(row, size.height)
        }
    }
}

/// No image: erase the disk and create partitions.
private struct ForgeFormatOnlyCard: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var forge = model.forge
        VStack(alignment: .leading, spacing: 14) {
            CardHeader(title: L("Solo formattazione", "Format only"),
                       subtitle: L("Cancella il disco e crea un nuovo filesystem.", "Erases the disk and creates a new filesystem."),
                       systemImage: "square.grid.3x3.square")
            Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 12) {
                GridRow {
                    Text(L("Filesystem", "Filesystem")).foregroundStyle(.secondary).gridColumnAlignment(.trailing)
                    ForgeFilesystemPicker(selection: Binding(get: { forge.plan.primary.fs }, set: { forge.plan.primary.setFilesystem($0) }))
                }
                GridRow(alignment: .top) {
                    Text(L("Nome", "Label")).foregroundStyle(.secondary).padding(.top, 4)
                    ForgeLabelField(fs: forge.plan.primary.fs, label: $forge.plan.primary.label)
                }
                GridRow {
                    Text(L("Schema", "Scheme")).foregroundStyle(.secondary)
                    Picker(L("Schema", "Scheme"), selection: $forge.plan.scheme) {
                        ForEach(ForgeScheme.allCases) { Text($0.title).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .frame(maxWidth: 300)
                    .accessibilityLabel(L("Schema delle partizioni", "Partition scheme"))
                }
            }
            .font(.callout)
            Text(forge.plan.scheme.hint).font(.caption).foregroundStyle(.secondary)
            Button {
                forge.drawerOpen = true
            } label: {
                Label(L("Cluster, seconda partizione e altre opzioni…", "Cluster size, second partition and more…"), systemImage: "sidebar.right")
            }
            .buttonStyle(.link)
            .font(.callout)
        }
        .card()
    }
}

struct ForgeFilesystemPicker: View {
    @Binding var selection: ForgeFS

    var body: some View {
        Picker(L("Filesystem", "Filesystem"), selection: $selection) {
            ForEach(ForgeFS.allCases) { fs in
                Text("\(fs.displayName) — \(fs.useHint)").tag(fs)
            }
        }
        .labelsHidden()
        .frame(maxWidth: 360)
        .accessibilityLabel(L("Filesystem", "Filesystem"))
    }
}

/// Label field with the filesystem's limit and a preview of what will be written.
struct ForgeLabelField: View {
    var fs: ForgeFS
    @Binding var label: String

    var body: some View {
        let sanitized = fs.sanitizedLabel(label)
        let (limit, _) = fs.labelLimit
        let used = fs.labelLength(label)
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                TextField(L("Senza nome", "Untitled"), text: $label)
                    .textFieldStyle(.roundedBorder)
                    .frame(maxWidth: 260)
                    .accessibilityLabel(L("Nome del volume", "Volume label"))
                Text("\(min(used, 999))/\(limit)")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(used > limit ? Color.orange : Color.secondary)
                    .accessibilityLabel(L("\(used) di \(limit)", "\(used) of \(limit)"))
            }
            Text(fs.labelRule).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            if sanitized != label && !label.isEmpty {
                Text(L("Verrà scritto come «\(sanitized)»", "Will be written as “\(sanitized)”"))
                    .font(.caption.weight(.medium)).foregroundStyle(.orange)
            }
        }
    }
}

private struct ForgeCreateActionBar: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var forge = model.forge
        let issues = forge.createIssues
        let disk = forge.targetDisk
        VStack(spacing: 0) {
            Divider()
            HStack(spacing: 12) {
                Image(systemName: disk?.symbol ?? "externaldrive.badge.questionmark")
                    .font(.system(size: 20))
                    .symbolRenderingMode(.hierarchical)
                    .foregroundStyle(disk == nil ? Color.secondary : Color.accentColor)
                VStack(alignment: .leading, spacing: 2) {
                    Text(disk.map { "\($0.title) · \(ByteFormat.string($0.size))" } ?? L("Nessun disco scelto", "No disk chosen"))
                        .font(.callout.weight(.semibold)).lineLimit(1)
                    Text(issues.first?.message ?? (forge.inspection == nil
                                                   ? L("Pronto per la formattazione", "Ready to format")
                                                   : L("Pronto per la scrittura", "Ready to write")))
                        .font(.caption).foregroundStyle(issues.isEmpty ? Color.ldGood : Color.secondary).lineLimit(2)
                }
                Spacer(minLength: 8)
                Button {
                    forge.drawerOpen.toggle()
                } label: {
                    Label(L("Opzioni avanzate", "Advanced options"), systemImage: "sidebar.right")
                }
                .help(L("Mostra o nascondi le opzioni avanzate", "Show or hide the advanced options"))
                Button(forge.inspection == nil ? L("Formatta…", "Format…") : L("Scrivi…", "Write…")) {
                    forge.prepareCreate()
                }
                .buttonStyle(CallToActionButtonStyle())
                .disabled(!issues.isEmpty)
                .keyboardShortcut(.defaultAction)
                .accessibilityHint(issues.first?.message ?? "")
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 12)
        }
        .background(.bar)
    }
}
