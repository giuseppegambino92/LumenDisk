import AppKit
import SwiftUI

// MARK: - Hash & Integrity

struct ForgeHashView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let forge = model.forge
        ForgeToolLayout(tool: .hash) {
            if let job = forge.jobs[.hash] {
                ForgeJobDashboard(job: job)
            } else {
                setup
            }
        }
    }

    private var setup: some View {
        @Bindable var forge = model.forge
        let draft = forge.hashDraft
        let ready: Bool = { if case .success = draft.request() { return true }; return false }()
        return VStack(spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    VStack(alignment: .leading, spacing: 14) {
                        CardHeader(title: L("Checksum e integrità", "Hash & Integrity"),
                                   subtitle: L("Calcola SHA-256, SHA-1 o MD5 di un'immagine o di un disco e confrontalo con quello pubblicato.",
                                               "Compute the SHA-256, SHA-1 or MD5 of an image or a disk and compare it with the published one."),
                                   systemImage: "number.square")
                        HStack(spacing: 10) {
                            Image(systemName: sourceSymbol).font(.system(size: 20)).symbolRenderingMode(.hierarchical).foregroundStyle(Color.accentColor).frame(width: 28)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(sourceTitle).font(.callout.weight(.semibold)).lineLimit(1).truncationMode(.middle)
                                Text(L("Un'immagine, oppure un disco dall'elenco a sinistra", "An image, or a disk from the list on the left"))
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                            Spacer()
                            Button(L("Scegli un file…", "Choose a file…")) {
                                if let url = ForgeImagePicker.choose(title: L("Scegli un'immagine", "Choose an image"), prompt: L("Scegli", "Choose")) {
                                    forge.hashDraft.source = .file(url.path)
                                }
                            }
                            .accessibilityLabel(L("Scegli il file di cui calcolare il checksum", "Choose the file to hash"))
                        }
                        .padding(12)
                        .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                        .dropDestination(for: URL.self) { urls, _ in
                            guard let url = urls.first else { return false }
                            forge.hashDraft.source = .file(url.path)
                            return true
                        }
                        HStack(spacing: 16) {
                            Text(L("Algoritmi", "Algorithms")).foregroundStyle(.secondary)
                            ForEach(HashAlgorithm.allCases) { algo in
                                Toggle(algo.title, isOn: Binding(get: { forge.hashDraft.algorithms.contains(algo) },
                                                                 set: { on in if on { forge.hashDraft.algorithms.insert(algo) } else { forge.hashDraft.algorithms.remove(algo) } }))
                                    .toggleStyle(.checkbox)
                            }
                        }
                        .font(.callout)
                        if case .file? = draft.source {
                            Toggle(L("Per un'immagine compressa calcola anche il checksum dell'immagine decompressa", "For a compressed image also hash the decompressed image"),
                                   isOn: $forge.hashDraft.decompressed)
                                .font(.callout)
                        }
                        VStack(alignment: .leading, spacing: 4) {
                            Text(L("Confronta con (facoltativo)", "Compare with (optional)")).font(.caption).foregroundStyle(.secondary)
                            TextField(L("Incolla il checksum pubblicato", "Paste the published checksum"), text: $forge.hashDraft.compareWith)
                                .textFieldStyle(.roundedBorder)
                                .font(.system(.callout, design: .monospaced))
                                .accessibilityLabel(L("Checksum da confrontare", "Checksum to compare"))
                            if ForgeValidation.normalizedChecksum(draft.compareWith) == nil {
                                Label(ForgeIssue.checksumInvalid.message, systemImage: "exclamationmark.circle.fill").font(.caption).foregroundStyle(.red)
                            }
                            Text(L("Se accanto all'immagine c'è un file SHA256SUMS, .sha256 o simile, viene controllato automaticamente.",
                                   "If a SHA256SUMS, .sha256 or similar file sits next to the image, it is checked automatically."))
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    .card()
                    if let error = forge.startErrors[.hash] { ForgeErrorBanner(message: error) }
                }
                .padding(22)
                .frame(maxWidth: 760)
                .frame(maxWidth: .infinity)
            }
            ActionBar {
                Spacer()
                Button(L("Calcola", "Compute")) { Task { await forge.startHash() } }
                    .buttonStyle(CallToActionButtonStyle())
                    .disabled(!ready)
            }
        }
    }

    private var sourceTitle: String {
        switch model.forge.hashDraft.source {
        case .file(let path)?: (path as NSString).lastPathComponent
        case .disk(let bsd)?: (model.forge.disk(bsd)?.title ?? bsd) + " · " + bsd
        case nil: L("Nessuna sorgente scelta", "No source chosen")
        }
    }

    private var sourceSymbol: String {
        switch model.forge.hashDraft.source {
        case .file?: "doc"
        case .disk?: "externaldrive"
        case nil: "questionmark.square.dashed"
        }
    }
}

// MARK: - Secure Wipe

struct ForgeWipeView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let forge = model.forge
        ForgeToolLayout(tool: .wipe) {
            if let job = forge.jobs[.wipe] {
                ForgeJobDashboard(job: job)
            } else {
                setup
            }
        }
    }

    private var setup: some View {
        @Bindable var forge = model.forge
        let disk = forge.disk(forge.wipeTarget)
        return VStack(spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    HStack(alignment: .top, spacing: 12) {
                        Image(systemName: "exclamationmark.triangle.fill").font(.title2).foregroundStyle(.orange)
                        VStack(alignment: .leading, spacing: 4) {
                            Text(L("SSD e memorie flash: la sovrascrittura non garantisce la cancellazione", "SSDs and flash memory: overwriting doesn't guarantee erasure"))
                                .font(.callout.weight(.semibold))
                            Text(L("Chiavette, schede SD e SSD spostano i dati tra celle di riserva che nessun programma può sovrascrivere: più passaggi non aiutano e consumano la memoria. Per questi supporti usa la crittografia o la cancellazione sicura del produttore.",
                                   "USB sticks, SD cards and SSDs move data between spare cells that no program can overwrite: more passes don't help and wear the memory out. For these devices use encryption or the manufacturer's secure erase."))
                                .font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                        }
                    }
                    .padding(14)
                    .background(Color.orange.opacity(0.09), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
                    .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous).strokeBorder(Color.orange.opacity(0.25), lineWidth: 1))
                    .accessibilityElement(children: .combine)

                    VStack(alignment: .leading, spacing: 12) {
                        CardHeader(title: L("Metodo", "Method"),
                                   subtitle: disk.map { L("Tempo stimato per \($0.title) (\(ByteFormat.string($0.size)))", "Estimated time for \($0.title) (\(ByteFormat.string($0.size)))") }
                                       ?? L("Scegli un disco per vedere il tempo stimato.", "Choose a disk to see the estimated time."),
                                   systemImage: "eraser.line.dashed")
                        ForEach(WipeMethod.allCases) { method in
                            Button {
                                forge.wipeMethod = method
                            } label: {
                                HStack {
                                    Image(systemName: forge.wipeMethod == method ? "largecircle.fill.circle" : "circle")
                                        .foregroundStyle(forge.wipeMethod == method ? Color.accentColor : .secondary)
                                    Text(method.title)
                                    Spacer()
                                    if let disk {
                                        Text("≈ " + ForgeFormat.duration(seconds: WipeEstimate.seconds(size: disk.size, method: method, verify: forge.wipeVerify,
                                                                                                   bytesPerSecond: ForgeDiskRules.nominalWriteSpeed(disk))))
                                            .foregroundStyle(.secondary).monospacedDigit()
                                    }
                                }
                                .padding(.vertical, 6)
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                            .accessibilityAddTraits(forge.wipeMethod == method ? .isSelected : [])
                            if method != WipeMethod.allCases.last { Divider() }
                        }
                        Toggle(L("Rileggi e controlla l'ultimo passaggio", "Read back and check the last pass"), isOn: $forge.wipeVerify)
                        if let disk {
                            Text(L("Stima a circa \(ForgeFormat.rate(ForgeDiskRules.nominalWriteSpeed(disk))) (\(disk.busProtocol)); la velocità reale dipende dal supporto.",
                                   "Estimated at about \(ForgeFormat.rate(ForgeDiskRules.nominalWriteSpeed(disk))) (\(disk.busProtocol)); the real speed depends on the device."))
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    .font(.callout)
                    .card()
                    if let error = forge.startErrors[.wipe] { ForgeErrorBanner(message: error) }
                }
                .padding(22)
                .frame(maxWidth: 760)
                .frame(maxWidth: .infinity)
            }
            ActionBar {
                Text(disk.map { "\($0.title) · \($0.bsd)" } ?? ForgeIssue.noTarget.message).font(.callout).foregroundStyle(.secondary)
                Spacer()
                Button(L("Cancella in modo sicuro…", "Wipe securely…")) { forge.prepareWipe() }
                    .buttonStyle(CallToActionButtonStyle(tint: .red))
                    .disabled(disk == nil)
            }
        }
    }
}

// MARK: - Partition Table Doctor

struct ForgeDoctorView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let forge = model.forge
        ForgeToolLayout(tool: .doctor) {
            if let job = forge.jobs[.doctor] {
                ForgeJobDashboard(job: job)
            } else {
                setup
            }
        }
    }

    private var setup: some View {
        let forge = model.forge
        let disk = forge.disk(forge.doctorTarget)
        return VStack(spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    VStack(alignment: .leading, spacing: 14) {
                        CardHeader(title: L("Doctor della tabella delle partizioni", "Partition Table Doctor"),
                                   subtitle: L("Controlla GPT, MBR e APM; ripara le copie danneggiate dalla copia di riserva; converte MBR ⇄ GPT senza spostare i dati.",
                                               "Checks GPT, MBR and APM; repairs damaged copies from the backup; converts MBR ⇄ GPT without moving data."),
                                   systemImage: "stethoscope")
                        VStack(alignment: .leading, spacing: 10) {
                            actionRow(.check, symbol: "magnifyingglass",
                                      text: L("Solo lettura: elenca i problemi trovati, con gravità e se sono riparabili.", "Read-only: lists the problems found, with severity and whether they can be repaired."), disk: disk)
                            Divider()
                            actionRow(.repair, symbol: "wrench.and.screwdriver",
                                      text: L("Ricostruisce intestazioni e tabelle danneggiate dalla loro copia; non tocca i dati.", "Rebuilds damaged headers and tables from their copy; doesn't touch the data."), disk: disk)
                            Divider()
                            actionRow(.convertToGpt, symbol: "arrow.triangle.2.circlepath",
                                      text: L("Da MBR a GPT, partizioni al loro posto.", "From MBR to GPT, partitions stay where they are."), disk: disk)
                            Divider()
                            actionRow(.convertToMbr, symbol: "arrow.triangle.2.circlepath",
                                      text: L("Da GPT a MBR (al massimo 4 partizioni entro 2 TiB).", "From GPT to MBR (at most 4 partitions within 2 TiB)."), disk: disk)
                        }
                    }
                    .card()
                    if let error = forge.startErrors[.doctor] { ForgeErrorBanner(message: error) }
                }
                .padding(22)
                .frame(maxWidth: 760)
                .frame(maxWidth: .infinity)
            }
            ActionBar {
                Text(disk.map { "\($0.title) · \($0.bsd)" } ?? ForgeIssue.noTarget.message).font(.callout).foregroundStyle(.secondary)
                Spacer()
            }
        }
    }

    private func actionRow(_ action: DoctorAction, symbol: String, text: String, disk: ForgeDisk?) -> some View {
        HStack(alignment: .center, spacing: 12) {
            Image(systemName: symbol).symbolRenderingMode(.hierarchical).foregroundStyle(Color.accentColor).frame(width: 22)
            VStack(alignment: .leading, spacing: 2) {
                Text(action.title).font(.callout.weight(.semibold))
                Text(text).font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 8)
            Button(action == .check ? L("Controlla", "Check") : action.title + "…") {
                model.forge.prepareDoctor(action)
            }
            .buttonStyle(.bordered)
            .tint(action == .check ? .accentColor : .orange)
            .disabled(disk == nil)
            .accessibilityLabel(action.title)
        }
    }
}
