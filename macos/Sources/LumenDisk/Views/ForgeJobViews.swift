import AppKit
import SwiftUI

/// Live telemetry of a job: progress, phase timeline, speed, ETA, log drawer; then the result.
struct ForgeJobDashboard: View {
    @Environment(AppModel.self) private var model
    var job: ForgeJob
    @State private var confirmCancel = false

    var body: some View {
        VStack(spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    header
                    if job.isRunning {
                        progressCard
                    } else {
                        ForgeResultCard(job: job)
                    }
                    if let phases = job.snapshot?.phases, !phases.isEmpty {
                        PhaseTimeline(phases: phases, current: job.snapshot?.phase, finished: !job.isRunning, state: job.state)
                    }
                }
                .padding(22)
                .frame(maxWidth: 760)
                .frame(maxWidth: .infinity)
            }
            ForgeLogDrawer(job: job)
        }
        .confirmationDialog(L("Interrompere l'operazione?", "Stop the operation?"), isPresented: $confirmCancel, titleVisibility: .visible) {
            Button(L("Interrompi", "Stop"), role: .destructive) {
                Task { await model.forge.cancel(job.tool) }
            }
            Button(L("Continua", "Keep going"), role: .cancel) {}
        } message: {
            Text(job.diskBSD != nil && job.kind != "hash" && job.kind != "doctor"
                 ? L("Il disco resterà a metà e andrà formattato o scritto di nuovo.", "The disk will be left half-written and will need formatting or writing again.")
                 : L("Il lavoro svolto finora andrà perso.", "The work done so far will be lost."))
        }
    }

    private var header: some View {
        HStack(alignment: .center, spacing: 12) {
            IconBadge(systemImage: symbol, tint: tint, size: 42)
            VStack(alignment: .leading, spacing: 2) {
                Text(job.title).font(.title3.weight(.semibold)).lineLimit(1).truncationMode(.middle)
                Text([job.diskTitle, job.diskBSD].compactMap { $0 }.joined(separator: " · "))
                    .font(.callout).foregroundStyle(.secondary)
            }
            Spacer(minLength: 8)
            if job.isRunning {
                Button(job.cancelRequested ? L("Interruzione…", "Stopping…") : L("Annulla…", "Cancel…"), role: .destructive) {
                    confirmCancel = true
                }
                .disabled(job.cancelRequested)
                .accessibilityLabel(L("Annulla l'operazione", "Cancel the operation"))
            }
        }
    }

    private var symbol: String {
        switch job.state {
        case .running: ForgeFormat.phaseSymbol(job.snapshot?.phase ?? .preparing)
        case .succeeded: "checkmark"
        case .failed: "xmark"
        case .cancelled: "stop.fill"
        }
    }

    private var tint: Color {
        switch job.state {
        case .running: .accentColor
        case .succeeded: .ldGood
        case .failed: .red
        case .cancelled: .ldSlate
        }
    }

    private var progressCard: some View {
        let s = job.snapshot
        let fraction = s?.fraction
        return VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .firstTextBaseline) {
                Text(ForgeFormat.phaseTitle(s?.phase ?? .preparing) + "…").font(.headline)
                Spacer()
                if let fraction {
                    Text(fraction.formatted(.percent.precision(.fractionLength(0)).locale(L10n.locale)))
                        .font(.system(.title2, design: .rounded).weight(.semibold))
                        .monospacedDigit()
                        .contentTransition(.numericText(value: fraction))
                }
            }
            Group {
                if let fraction {
                    ProgressView(value: fraction)
                } else {
                    ProgressView()
                }
            }
            .progressViewStyle(.linear)
            .animation(.smooth(duration: 0.3), value: fraction)
            .accessibilityLabel(L("Avanzamento", "Progress"))
            HStack(spacing: 10) {
                StatTile(title: L("Velocità", "Speed"), value: ForgeFormat.rate(s?.rate ?? 0, unit: s?.unit ?? "bytes"), symbol: "speedometer")
                StatTile(title: L("Tempo rimanente", "Time left"), value: ForgeFormat.duration(s?.etaSecs), symbol: "hourglass")
                StatTile(title: L("Trascorso", "Elapsed"), value: ForgeFormat.elapsed(s?.elapsedSecs ?? 0), symbol: "timer")
                StatTile(title: L("Avanzamento", "Progress"), value: ForgeFormat.amount(done: s?.done ?? 0, total: s?.total ?? 0, unit: s?.unit ?? "bytes"), symbol: "chart.bar.fill")
            }
        }
        .card()
    }
}

private struct StatTile: View {
    var title: String
    var value: String
    var symbol: String

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Label(title, systemImage: symbol).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            Text(value)
                .font(.system(.callout, design: .rounded).weight(.semibold))
                .monospacedDigit()
                .lineLimit(1)
                .minimumScaleFactor(0.7)
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .accessibilityElement(children: .combine)
    }
}

/// The phases the job announced, in order: done ones ticked, the current one highlighted.
struct PhaseTimeline: View {
    var phases: [ForgePhase]
    var current: ForgePhase?
    var finished: Bool
    var state: ForgeJobState

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(L("Fasi", "Phases")).font(.caption.weight(.semibold)).foregroundStyle(.secondary)
            ChipFlowLayout(spacing: 6) {
                ForEach(ForgeFormat.timeline(phases)) { phase in
                    let isCurrent = !finished && phase == current
                    let failedHere = finished && state != .succeeded && phase == current
                    HStack(spacing: 5) {
                        if isCurrent {
                            ProgressView().controlSize(.mini)
                        } else {
                            Image(systemName: failedHere ? (state == .cancelled ? "stop.circle.fill" : "xmark.circle.fill") : "checkmark.circle.fill")
                                .foregroundStyle(failedHere ? (state == .cancelled ? Color.secondary : Color.red) : Color.ldGood)
                        }
                        Text(ForgeFormat.phaseTitle(phase))
                    }
                    .font(.caption.weight(isCurrent ? .semibold : .regular))
                    .padding(.horizontal, 9)
                    .padding(.vertical, 5)
                    .background(isCurrent ? Color.accentColor.opacity(0.14) : Color.ldWell, in: Capsule())
                    .accessibilityElement(children: .combine)
                    .accessibilityValue(isCurrent ? L("In corso", "In progress") : failedHere ? L("Interrotta", "Stopped") : L("Completata", "Done"))
                }
            }
        }
        .card(padding: 16)
    }
}

/// Outcome of a finished job.
struct ForgeResultCard: View {
    @Environment(AppModel.self) private var model
    var job: ForgeJob

    var body: some View {
        let rows = ForgeSummary.rows(kind: job.kind, result: job.snapshot?.result)
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 10) {
                Image(systemName: icon).font(.system(size: 26)).foregroundStyle(tint)
                VStack(alignment: .leading, spacing: 2) {
                    Text(title).font(.headline)
                    if let elapsed = job.snapshot?.elapsedSecs {
                        Text(L("Durata: \(ForgeFormat.duration(seconds: elapsed))", "Took \(ForgeFormat.duration(seconds: elapsed))"))
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                Spacer()
                Button(L("Fine", "Done")) {
                    Task { await model.forge.dismiss(job.tool) }
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
            }
            if let error = job.errorMessage, job.state == .failed {
                ForgeErrorBanner(message: ForgeFormat.friendlyError(error))
            }
            if job.kind == "hash" { hashComparison }
            if job.kind == "doctor", let report = DoctorReport.from(result: job.snapshot?.result) {
                DoctorFindingsList(report: report)
            }
            if !rows.isEmpty {
                Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 7) {
                    ForEach(rows) { row in
                        GridRow(alignment: .firstTextBaseline) {
                            Text(row.title).foregroundStyle(.secondary).gridColumnAlignment(.trailing)
                            Text(row.value)
                                .font(row.tone == .mono ? .system(.caption, design: .monospaced) : .callout)
                                .foregroundStyle(row.tone == .good ? Color.ldGood : row.tone == .bad ? Color.red : Color.primary)
                                .textSelection(.enabled)
                                .lineLimit(row.tone == .mono ? 1 : 3)
                                .truncationMode(.middle)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                        .font(.callout)
                    }
                }
            }
        }
        .card()
    }

    @ViewBuilder private var hashComparison: some View {
        if let cmp = HashDraft.comparison(pasted: model.forge.hashDraft.compareWith, result: job.snapshot?.result) {
            Label(cmp.matches ? L("\(cmp.algorithm.title) corrisponde al valore incollato", "\(cmp.algorithm.title) matches the pasted value")
                              : L("\(cmp.algorithm.title) NON corrisponde al valore incollato", "\(cmp.algorithm.title) does NOT match the pasted value"),
                  systemImage: cmp.matches ? "checkmark.seal.fill" : "xmark.seal.fill")
                .font(.callout.weight(.semibold))
                .foregroundStyle(cmp.matches ? Color.ldGood : Color.red)
        }
    }

    private var icon: String {
        switch job.state {
        case .succeeded: "checkmark.circle.fill"
        case .failed: "xmark.octagon.fill"
        case .cancelled: "stop.circle.fill"
        case .running: "circle.dotted"
        }
    }

    private var tint: Color {
        switch job.state {
        case .succeeded: .ldGood
        case .failed: .red
        default: .secondary
        }
    }

    private var title: String {
        switch job.state {
        case .succeeded: L("Completato", "Completed")
        case .failed: L("Non riuscito", "Failed")
        case .cancelled: L("Annullato", "Cancelled")
        case .running: L("In corso", "Running")
        }
    }
}

struct DoctorFindingsList: View {
    var report: DoctorReport

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if report.findings.isEmpty {
                Label(L("Nessun problema: la tabella delle partizioni è integra.", "No problems: the partition table is healthy."), systemImage: "checkmark.seal.fill")
                    .foregroundStyle(Color.ldGood).font(.callout.weight(.medium))
            }
            ForEach(report.findings.sorted { $0.severity > $1.severity }) { f in
                HStack(alignment: .top, spacing: 10) {
                    Image(systemName: f.severity == .error ? "xmark.octagon.fill" : f.severity == .warning ? "exclamationmark.triangle.fill" : "info.circle.fill")
                        .foregroundStyle(f.severity == .error ? Color.red : f.severity == .warning ? Color.orange : Color.secondary)
                    VStack(alignment: .leading, spacing: 3) {
                        Text(f.message).font(.callout).fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
                        HStack(spacing: 6) {
                            Text(f.code).font(.caption.monospaced()).foregroundStyle(.tertiary)
                            if f.repairable {
                                StatusChip(title: L("Riparabile", "Repairable"), systemImage: "wrench.and.screwdriver", tint: .ldGood)
                            }
                        }
                    }
                }
                .accessibilityElement(children: .combine)
            }
            if !report.partitions.isEmpty {
                Divider()
                ForEach(report.partitions, id: \.self) { p in
                    HStack(spacing: 8) {
                        TagBadge(text: "\(p.index)")
                        Text(p.name.isEmpty ? p.type : p.name).font(.callout).lineLimit(1).truncationMode(.middle)
                        Spacer()
                        Text(ByteFormat.string(p.size)).font(.callout).foregroundStyle(.secondary).monospacedDigit()
                    }
                }
            }
        }
        .padding(12)
        .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }
}

/// Pull-out drawer at the bottom with the core's log lines.
struct ForgeLogDrawer: View {
    @Environment(AppModel.self) private var model
    var job: ForgeJob

    var body: some View {
        @Bindable var forge = model.forge
        VStack(spacing: 0) {
            Divider()
            Button {
                withAnimation(.snappy) { forge.logDrawerOpen.toggle() }
            } label: {
                HStack(spacing: 8) {
                    Image(systemName: "chevron.up")
                        .font(.caption.weight(.semibold))
                        .rotationEffect(.degrees(forge.logDrawerOpen ? 180 : 0))
                    Text(L("Registro", "Log")).font(.callout.weight(.medium))
                    Text("\(job.logs.count)").font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                    if let last = job.logs.last, !forge.logDrawerOpen {
                        Text(last.msg).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.tail)
                    }
                    Spacer()
                }
                .padding(.horizontal, 20)
                .padding(.vertical, 9)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(forge.logDrawerOpen ? L("Nascondi il registro", "Hide the log") : L("Mostra il registro", "Show the log"))
            if forge.logDrawerOpen {
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 2) {
                            ForEach(job.logs) { line in
                                HStack(alignment: .firstTextBaseline, spacing: 8) {
                                    Text(String(format: "%6.1f s", Double(line.ms) / 1000))
                                        .foregroundStyle(.tertiary)
                                    Text(line.msg)
                                        .foregroundStyle(line.level == .error ? Color.red : line.level == .warn ? Color.orange : Color.primary)
                                        .textSelection(.enabled)
                                        .fixedSize(horizontal: false, vertical: true)
                                }
                                .font(.system(size: 11, design: .monospaced))
                                .id(line.seq)
                            }
                            if job.logs.isEmpty {
                                Text(L("Nessun messaggio.", "No messages.")).font(.caption).foregroundStyle(.secondary)
                            }
                        }
                        .padding(.horizontal, 20)
                        .padding(.vertical, 8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .frame(height: 190)
                    .background(Color.ldWell)
                    .onChange(of: job.lastSeq, initial: true) {
                        if let last = job.logs.last { proxy.scrollTo(last.seq, anchor: .bottom) }
                    }
                }
                .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        }
        .background(.bar)
    }
}

// MARK: - Destructive confirmation

struct ForgeConfirmSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    var confirmation: ForgeConfirmation

    var body: some View {
        @Bindable var forge = model.forge
        let disk = confirmation.disk
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top, spacing: 14) {
                IconBadge(systemImage: confirmation.erasesData ? "exclamationmark.triangle.fill" : "wrench.and.screwdriver.fill",
                          tint: confirmation.erasesData ? .red : .orange, size: 44)
                VStack(alignment: .leading, spacing: 4) {
                    Text(confirmation.headline).font(.title3.weight(.semibold)).fixedSize(horizontal: false, vertical: true)
                    Text(confirmation.erasesData
                         ? L("Tutti i dati su questo disco verranno cancellati definitivamente.", "All data on this disk will be erased permanently.")
                         : L("La tabella delle partizioni verrà riscritta.", "The partition table will be rewritten."))
                        .font(.callout).foregroundStyle(.secondary)
                }
            }
            VStack(alignment: .leading, spacing: 8) {
                Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 6) {
                    InfoRow(title: L("Disco", "Disk"), value: disk.title)
                    InfoRow(title: L("Dimensione", "Size"), value: ByteFormat.string(disk.size))
                    InfoRow(title: L("Dispositivo", "Device"), value: "/dev/\(disk.bsd)")
                    InfoRow(title: L("Collegamento", "Connection"), value: disk.busProtocol.isEmpty ? "—" : disk.busProtocol)
                }
                Divider()
                Text(confirmation.erasesData ? L("Volumi che verranno cancellati", "Volumes that will be erased") : L("Volumi che verranno smontati", "Volumes that will be unmounted"))
                    .font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                if disk.volumeNames.isEmpty {
                    Text(L("Nessun volume montato", "No mounted volume")).font(.callout).foregroundStyle(.secondary)
                } else {
                    ForEach(disk.volumeNames, id: \.self) { name in
                        Label(name, systemImage: "internaldrive").font(.callout.weight(.medium))
                    }
                }
            }
            .padding(12)
            .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            VStack(alignment: .leading, spacing: 4) {
                ForEach(confirmation.details, id: \.self) { line in
                    Label(line, systemImage: "arrow.right.circle").font(.callout).fixedSize(horizontal: false, vertical: true)
                }
            }
            Toggle(isOn: $forge.confirmationAcknowledged) {
                Text(confirmation.erasesData
                     ? L("Ho capito che tutto ciò che c'è su «\(disk.title)» (\(disk.bsd)) andrà perso", "I understand that everything on “\(disk.title)” (\(disk.bsd)) will be lost")
                     : L("Ho capito che la tabella delle partizioni di \(disk.bsd) verrà modificata", "I understand that the partition table of \(disk.bsd) will be changed"))
                    .fixedSize(horizontal: false, vertical: true)
            }
            .toggleStyle(.checkbox)
            .accessibilityHint(L("Obbligatorio per continuare", "Required to continue"))
            HStack {
                Spacer()
                Button(L("Annulla", "Cancel"), role: .cancel) {
                    forge.confirmation = nil
                    dismiss()
                }
                .keyboardShortcut(.cancelAction)
                Button(confirmation.actionTitle, role: .destructive) {
                    Task { await forge.confirmPending() }
                }
                .buttonStyle(CallToActionButtonStyle(tint: confirmation.erasesData ? .red : .orange))
                .disabled(!forge.confirmationAcknowledged)
                .accessibilityHint(L("Avvia l'operazione", "Starts the operation"))
            }
        }
        .padding(24)
        .frame(width: 500)
    }
}
