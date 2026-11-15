import SwiftUI

/// "Salute e Velocità Disco" card: one-click speed test through the mounted volume and, for
/// physical devices, the hardware / SMART report in plain language.
struct DiskHealthCard: View {
    @Environment(AppModel.self) private var model
    /// Mounted volume (enables the speed test); nil for a partition LumenDisk has not mounted.
    var volume: VolumeStatus?
    /// BSD name of the physical device, nil for image files and remote volumes.
    var bsdName: String?

    private var record: SpeedTestRecord? { volume.flatMap { model.speedTest(for: $0) } }
    private var healthLoad: DiskHealthLoad? { bsdName.flatMap { model.diskHealth(bsdName: $0) } }
    private var health: DiskHealth? {
        if case .loaded(let health)? = healthLoad { return health }
        return nil
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            header
            if let volume { speedSection(volume) }
            if bsdName != nil {
                if volume != nil { Divider() }
                hardwareSection
            }
        }
        .card()
        .task(id: bsdName) {
            if let bsdName { await model.loadDiskHealth(bsdName: bsdName) }
        }
    }

    // MARK: Header

    private var header: some View {
        HStack(alignment: .center, spacing: 14) {
            IconBadge(systemImage: "gauge.with.dots.needle.67percent", tint: .ldGood, size: 40)
            VStack(alignment: .leading, spacing: 2) {
                Text(L("Salute e Velocità Disco", "Disk Health & Speed")).font(.headline)
                Text(subtitle)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 12)
            if let volume { speedButton(volume) }
        }
    }

    private var subtitle: String {
        if volume == nil {
            return L("Come è collegato il disco e cosa dice di sé.", "How the disk is connected and what it reports about itself.")
        }
        if bsdName == nil {
            return L("Misura la velocità reale di lettura e scrittura in circa 3 secondi.",
                     "Measures the real read and write speed in about 3 seconds.")
        }
        return L("Velocità reale, collegamento e stato dell'hardware.", "Real speed, connection and hardware status.")
    }

    private func speedButton(_ volume: VolumeStatus) -> some View {
        let running = record?.isRunning == true
        return Button {
            Task { await model.runSpeedTest(volume) }
        } label: {
            Label(record?.result == nil ? L("Test di Velocità", "Speed Test") : L("Ripeti il Test", "Run Again"),
                  systemImage: "speedometer")
        }
        .controlSize(.large)
        .disabled(running || volume.readOnly || model.activity(for: volume.id) != nil)
        .help(volume.readOnly ? readOnlyExplanation : L("Scrive e rilegge un file temporaneo nascosto, poi lo elimina.",
                                                          "Writes and reads back a hidden temporary file, then deletes it."))
    }

    private var readOnlyExplanation: String {
        L("Il test di velocità è disattivato perché il volume è in sola lettura: per misurare la scrittura deve creare un file temporaneo.",
          "The speed test is off because the volume is read-only: measuring write speed needs a temporary file.")
    }

    // MARK: Speed

    @ViewBuilder
    private func speedSection(_ volume: VolumeStatus) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            if let record, record.isRunning {
                runningState(startedAt: record.startedAt ?? Date())
            } else if let result = record?.result {
                resultPill(result)
                HStack(spacing: 6) {
                    Text(result.randomSummary)
                    if let date = record?.date {
                        Text("·")
                        Text(L("misurato \(date.relativeDescription)", "measured \(date.relativeDescription)"))
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
                .monospacedDigit()
                .padding(.leading, 4)
            } else if record?.error == nil, !volume.readOnly {
                Text(L("Premi «Test di Velocità» per scoprire a cosa è adatto questo disco.",
                       "Click “Speed Test” to find out what this disk is good for."))
                    .font(.callout)
                    .foregroundStyle(.tertiary)
            }
            if let error = record?.error, record?.isRunning != true {
                Label(error, systemImage: "exclamationmark.triangle")
                    .font(.callout)
                    .foregroundStyle(Color.ldAmber)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if volume.readOnly {
                Label(readOnlyExplanation, systemImage: "lock")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .animation(.snappy, value: record)
    }

    private func runningState(startedAt: Date) -> some View {
        TimelineView(.animation(minimumInterval: 1.0 / 30)) { context in
            let elapsed = context.date.timeIntervalSince(startedAt)
            // Eases towards 95 % over the ≈3 s budget; never claims to be done before it is.
            let progress = min(0.95, 1 - exp(-elapsed / 1.4))
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text(phase(elapsed))
                        .font(.callout.weight(.medium))
                        .foregroundStyle(.secondary)
                    Spacer()
                }
                ProgressView(value: progress)
                    .progressViewStyle(.linear)
                    .tint(Color.ldGood)
            }
        }
        .frame(maxWidth: 520, alignment: .leading)
    }

    private func phase(_ elapsed: TimeInterval) -> String {
        switch elapsed {
        case ..<1.3: L("Misuro la scrittura…", "Measuring writes…")
        case ..<2.4: L("Misuro la lettura…", "Measuring reads…")
        default: L("Misuro i piccoli file…", "Measuring small files…")
        }
    }

    private func resultPill(_ result: BenchmarkResult) -> some View {
        let verdict = result.verdict
        let tint = verdict.tint
        return HStack(spacing: 8) {
            Image(systemName: verdict.symbol)
                .font(.system(size: 12, weight: .semibold))
            (Text(result.summary).fontWeight(.semibold) + Text(" · " + verdict.text))
                .font(.callout)
                .monospacedDigit()
                .fixedSize(horizontal: false, vertical: true)
        }
        .foregroundStyle(tint)
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(tint.opacity(0.11), in: Capsule())
        .accessibilityElement(children: .combine)
    }

    // MARK: Hardware

    @ViewBuilder
    private var hardwareSection: some View {
        switch healthLoad {
        case .loaded(let health)?:
            VStack(alignment: .leading, spacing: 12) {
                Text(L("Hardware", "Hardware")).font(.subheadline.weight(.semibold)).foregroundStyle(.secondary)
                Grid(horizontalSpacing: 10, verticalSpacing: 10) {
                    GridRow {
                        HardwareTile(title: L("Collegamento", "Connection"), value: health.connectionText,
                                     systemImage: health.connectionSymbol, tint: .ldIndigo)
                        HardwareTile(title: L("Tipo di disco", "Disk type"), value: health.mediaTypeText,
                                     systemImage: health.mediaTypeSymbol, tint: .ldSlate)
                    }
                    GridRow {
                        HardwareTile(title: L("Stato SMART", "SMART status"), value: health.smartText,
                                     systemImage: smartSymbol(health), tint: smartTint(health))
                        HardwareTile(title: L("Capacità e modello", "Capacity and model"), value: capacityText(health),
                                     systemImage: "info.circle.fill", tint: .ldPlum)
                    }
                }
                if health.smartState == .failing {
                    Label(L("Il disco segnala guasti imminenti: copia subito i tuoi file su un altro disco e non usarlo per dati importanti.",
                            "The disk reports imminent failures: copy your files to another disk right away and don’t use it for important data."),
                          systemImage: "exclamationmark.octagon.fill")
                        .font(.callout.weight(.medium))
                        .foregroundStyle(.red)
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(Color.red.opacity(0.08), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        case .failed(let message)?:
            Label(L("Informazioni sull'hardware non disponibili: \(message)", "Hardware information unavailable: \(message)"),
                  systemImage: "questionmark.circle")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        case .loading?, nil:
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text(L("Lettura delle informazioni sull'hardware…", "Reading hardware information…"))
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func smartSymbol(_ health: DiskHealth) -> String {
        switch health.smartState {
        case .verified: "checkmark.seal.fill"
        case .failing: "exclamationmark.triangle.fill"
        case .unavailable: "minus.circle.fill"
        }
    }

    private func smartTint(_ health: DiskHealth) -> Color {
        switch health.smartState {
        case .verified: .ldGood
        case .failing: .red
        case .unavailable: .gray
        }
    }

    private func capacityText(_ health: DiskHealth) -> String {
        var parts: [String] = []
        if let size = health.size { parts.append(ByteFormat.string(size)) }
        if let model = health.model { parts.append(model) }
        return parts.isEmpty ? L("Non indicato", "Not reported") : parts.joined(separator: " · ")
    }
}

extension BenchmarkResult.Verdict {
    var tint: Color {
        switch self {
        case .video4K: .ldGood
        case .photos: .ldIndigo
        case .documents: .ldAmber
        case .archives: .ldPlum
        }
    }
}

private struct HardwareTile: View {
    var title: String
    var value: String
    var systemImage: String
    var tint: Color

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: systemImage)
                .font(.system(size: 16, weight: .medium))
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(tint)
                .frame(width: 22)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.caption).foregroundStyle(.secondary)
                Text(value)
                    .font(.system(.callout, design: .rounded).weight(.semibold))
                    .foregroundStyle(tint == .red ? Color.red : Color.primary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
        }
        .padding(12)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .accessibilityElement(children: .combine)
    }
}

extension DiskHealth {
    /// Rows for the collapsible "Dettagli tecnici" (raw facts and smartctl counters).
    var technicalRows: [TechnicalRow] {
        var rows: [TechnicalRow] = []
        if let proto = `protocol` { rows.append(TechnicalRow(title: L("Protocollo", "Protocol"), value: proto)) }
        if let status = smartStatus { rows.append(TechnicalRow(title: "SMART (macOS)", value: status)) }
        if let location = location { rows.append(TechnicalRow(title: L("Posizione", "Location"), value: location)) }
        if let report = smartctl {
            if let t = report.temperatureC { rows.append(TechnicalRow(title: L("Temperatura", "Temperature"), value: "\(t) °C")) }
            if let h = report.powerOnHours {
                rows.append(TechnicalRow(title: L("Ore di accensione", "Power-on hours"),
                                         value: BenchmarkResult.grouped(h) + L(" ore", " hours")))
            }
            if let r = report.reallocatedSectors { rows.append(TechnicalRow(title: L("Settori riallocati", "Reallocated sectors"), value: "\(r)")) }
            if let m = report.mediaErrors { rows.append(TechnicalRow(title: L("Errori del supporto", "Media errors"), value: "\(m)")) }
            if let u = report.percentageUsed { rows.append(TechnicalRow(title: L("Usura dichiarata", "Reported wear"), value: "\(u) %")) }
        }
        return rows
    }
}
