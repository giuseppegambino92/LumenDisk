import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// Collapsible side panel with every option of Create & Format. Shows the sections that matter
/// for the chosen image; "Show all" reveals the others too.
struct ForgeDrawer: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var forge = model.forge
        let inspection = forge.inspection
        let mode = forge.plan.effectiveMode(inspection)
        let all = forge.showAllSections
        VStack(spacing: 0) {
            HStack {
                Text(L("Opzioni avanzate", "Advanced options")).font(.headline)
                Spacer()
                Toggle(L("Mostra tutte", "Show all"), isOn: $forge.showAllSections)
                    .toggleStyle(.switch)
                    .controlSize(.mini)
                    .font(.caption)
                Button {
                    forge.drawerOpen = false
                } label: {
                    Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
                .help(L("Chiudi", "Close"))
                .accessibilityLabel(L("Chiudi le opzioni avanzate", "Close the advanced options"))
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    if inspection != nil || all {
                        DrawerSection(title: L("Modalità di scrittura", "Write mode"), symbol: "square.and.arrow.down") { WriteModeSection() }
                        DrawerSection(title: L("Verifica e checksum", "Verification & checksum"), symbol: "checkmark.shield") { VerifySection() }
                    }
                    if inspection == nil || all {
                        DrawerSection(title: L("Formattazione", "Formatting"), symbol: "square.grid.3x3.square") { FormatSection() }
                    }
                    if all || mode == .windows || inspection?.windows != nil {
                        DrawerSection(title: "Windows", symbol: "pc", inactiveNote: mode == .windows ? nil : inactive) { WindowsSection() }
                    }
                    if all || mode == .iso || inspection?.linux != nil {
                        DrawerSection(title: "Linux", symbol: "terminal", inactiveNote: mode == .iso ? nil : inactive) { LinuxSection() }
                    }
                    if all || inspection?.rpi != nil {
                        DrawerSection(title: "Raspberry Pi", symbol: "cpu", inactiveNote: mode == .dd ? nil : inactive) { RaspberrySection() }
                    }
                }
                .padding(14)
                .frame(width: ForgeDrawer.width, alignment: .leading)
            }
        }
    }

    static let width: CGFloat = 340

    private var inactive: String {
        L("Non usate con la modalità scelta.", "Not used with the chosen mode.")
    }
}

/// Toggle whose label wraps instead of widening the drawer.
private struct WrapToggle: View {
    var title: String
    @Binding var isOn: Bool

    init(_ title: String, isOn: Binding<Bool>) {
        self.title = title
        self._isOn = isOn
    }

    var body: some View {
        Toggle(isOn: $isOn) {
            Text(title).fixedSize(horizontal: false, vertical: true)
        }
        .accessibilityLabel(title)
    }
}

/// A titled, collapsible group of the drawer.
private struct DrawerSection<Content: View>: View {
    var title: String
    var symbol: String
    var inactiveNote: String?
    @ViewBuilder var content: () -> Content
    @State private var expanded = true

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Button {
                withAnimation(.snappy) { expanded.toggle() }
            } label: {
                HStack(spacing: 8) {
                    Image(systemName: symbol).symbolRenderingMode(.hierarchical).foregroundStyle(Color.accentColor).frame(width: 18)
                    Text(title).font(.callout.weight(.semibold))
                    Spacer()
                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                        .rotationEffect(.degrees(expanded ? 90 : 0))
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(title)
            .accessibilityValue(expanded ? L("Espanso", "Expanded") : L("Compresso", "Collapsed"))
            if expanded {
                if let inactiveNote {
                    Label(inactiveNote, systemImage: "info.circle").font(.caption).foregroundStyle(.secondary)
                }
                content()
                    .font(.callout)
                    .opacity(inactiveNote == nil ? 1 : 0.75)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.ldCard, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(Color.ldBorder, lineWidth: 1))
    }
}

/// Inline validation message under a field.
private struct FieldIssue: View {
    var issue: ForgeIssue?

    var body: some View {
        if let issue {
            Label(issue.message, systemImage: "exclamationmark.circle.fill")
                .font(.caption)
                .foregroundStyle(.red)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct LabeledField<Content: View>: View {
    var title: String
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.caption).foregroundStyle(.secondary)
            content()
        }
    }
}

// MARK: Sections

private struct WriteModeSection: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var forge = model.forge
        let recommended = forge.inspection?.recommendedMode
        VStack(alignment: .leading, spacing: 6) {
            Picker(L("Modalità di scrittura", "Write mode"), selection: $forge.plan.mode) {
                ForEach(ForgeWriteMode.allCases) { m in
                    VStack(alignment: .leading, spacing: 1) {
                        Text(m.title + (m == recommended ? " · " + L("consigliata", "recommended") : ""))
                        Text(m.hint).font(.caption).foregroundStyle(.secondary)
                    }
                    .tag(m)
                }
            }
            .pickerStyle(.radioGroup)
            .labelsHidden()
            .accessibilityLabel(L("Modalità di scrittura", "Write mode"))
        }
    }
}

private struct VerifySection: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var forge = model.forge
        let checksumOK = ForgeValidation.normalizedChecksum(forge.plan.expectedChecksum) != nil
        VStack(alignment: .leading, spacing: 10) {
            WrapToggle(L("Verifica dopo la scrittura", "Verify after writing"), isOn: $forge.plan.verify)
            LabeledField(title: L("Checksum atteso (facoltativo)", "Expected checksum (optional)")) {
                TextField(L("Incolla SHA-256, SHA-1 o MD5", "Paste SHA-256, SHA-1 or MD5"), text: $forge.plan.expectedChecksum)
                    .textFieldStyle(.roundedBorder)
                    .font(.system(.caption, design: .monospaced))
                    .accessibilityLabel(L("Checksum atteso", "Expected checksum"))
                FieldIssue(issue: checksumOK ? nil : .checksumInvalid)
                if forge.plan.expectedChecksum.isEmpty, let sums = forge.inspection?.checksumFile {
                    Text(L("Senza un valore qui viene usato \((sums.path as NSString).lastPathComponent).", "Without a value here \((sums.path as NSString).lastPathComponent) is used."))
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
            WrapToggle(L("Espelli al termine", "Eject when done"), isOn: $forge.plan.eject)
        }
    }
}

private struct FormatSection: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var forge = model.forge
        let disk = forge.targetDisk
        VStack(alignment: .leading, spacing: 12) {
            LabeledField(title: L("Schema delle partizioni", "Partition scheme")) {
                Picker(L("Schema", "Scheme"), selection: $forge.plan.scheme) {
                    ForEach(ForgeScheme.allCases) { Text($0.title).tag($0) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .accessibilityLabel(L("Schema delle partizioni", "Partition scheme"))
                Text(forge.plan.scheme.hint).font(.caption).foregroundStyle(.secondary)
            }
            PartitionEditor(title: L("Partizione 1", "Partition 1"), draft: $forge.plan.primary)
            if forge.plan.scheme != .none {
                WrapToggle(L("Aggiungi una seconda partizione", "Add a second partition"), isOn: $forge.plan.addSecond)
                if forge.plan.addSecond {
                    LabeledField(title: L("Dimensione della partizione 1", "Size of partition 1")) {
                        SizeStepper(bytes: $forge.plan.primarySize, maximum: disk.map { $0.size > (128 << 20) ? $0.size - (128 << 20) : 0 })
                    }
                    PartitionEditor(title: L("Partizione 2 (resto del disco)", "Partition 2 (rest of the disk)"), draft: $forge.plan.secondary)
                    FieldIssue(issue: forge.plan.formatIssues(disk: disk).first { $0 == .firstPartitionSize })
                }
            }
        }
    }
}

private struct PartitionEditor: View {
    var title: String
    @Binding var draft: PartitionDraft

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.caption.weight(.semibold)).foregroundStyle(.secondary)
            ForgeFilesystemPicker(selection: Binding(get: { draft.fs }, set: { draft.setFilesystem($0) }))
            ForgeLabelField(fs: draft.fs, label: $draft.label)
            LabeledField(title: draft.fs.clusterTitle) {
                Picker(draft.fs.clusterTitle, selection: $draft.clusterSize) {
                    Text(L("Predefinita", "Default")).tag(UInt32?.none)
                    ForEach(draft.fs.clusterSizes, id: \.self) { size in
                        Text(ForgeFormat.cluster(size)).tag(UInt32?.some(size))
                    }
                }
                .labelsHidden()
                .disabled(draft.fs.clusterSizes.isEmpty)
                .accessibilityLabel(draft.fs.clusterTitle)
                if draft.fs.clusterSizes.isEmpty {
                    Text(L("F2FS usa sempre blocchi da 4 KiB.", "F2FS always uses 4 KiB blocks.")).font(.caption).foregroundStyle(.secondary)
                }
            }
        }
        .padding(10)
        .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }
}

/// GB stepper bounded by the disk.
private struct SizeStepper: View {
    @Binding var bytes: UInt64
    var maximum: UInt64?

    var body: some View {
        let gib = Double(bytes) / Double(1 << 30)
        let maxGiB = maximum.map { Double($0) / Double(1 << 30) } ?? 2048
        HStack {
            Slider(value: Binding(get: { min(gib, maxGiB) }, set: { bytes = UInt64(max(0.25, $0) * Double(1 << 30)) }),
                   in: 0.25...max(0.5, maxGiB))
                .accessibilityLabel(L("Dimensione", "Size"))
            Text(ByteFormat.string(bytes)).font(.callout.monospacedDigit()).frame(minWidth: 70, alignment: .trailing)
        }
    }
}

private struct WindowsSection: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var forge = model.forge
        let w = forge.plan.windows
        VStack(alignment: .leading, spacing: 12) {
            if let info = forge.inspection?.windows, info.needsSplit {
                Text(info.splitBlocker ?? L("install.wim supera 4 GB: con FAT32 viene diviso in parti .swm.", "install.wim is over 4 GB: on FAT32 it is split into .swm parts."))
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            LabeledField(title: L("Struttura", "Layout")) {
                Picker(L("Struttura", "Layout"), selection: $forge.plan.windows.layout) {
                    Text(L("Automatica", "Automatic") + recommendedLayoutSuffix).tag(Choice<WindowsLayout>.automatic)
                    ForEach(WindowsLayout.allCases) { Text($0.title).tag(Choice.value($0)) }
                }
                .labelsHidden()
                .accessibilityLabel(L("Struttura del supporto Windows", "Windows media layout"))
            }
            LabeledField(title: L("Avvio", "Boot")) {
                Picker(L("Avvio", "Boot"), selection: $forge.plan.windows.boot) {
                    Text(L("Automatico (UEFI e BIOS)", "Automatic (UEFI and BIOS)")).tag(Choice<BootTarget>.automatic)
                    ForEach(BootTarget.allCases) { Text($0.title).tag(Choice.value($0)) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .accessibilityLabel(L("Destinazione di avvio", "Boot target"))
            }
            Divider()
            WrapToggle(L("Usa un file autounattend.xml personalizzato", "Inject a custom autounattend.xml"), isOn: $forge.plan.windows.useAnswerFile)
            if w.useAnswerFile {
                HStack {
                    Text(w.answerFileName ?? L("Nessun file scelto", "No file chosen")).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                    Spacer()
                    Button(L("Scegli…", "Choose…")) { pickAnswerFile() }
                        .accessibilityLabel(L("Scegli il file autounattend.xml", "Choose the autounattend.xml file"))
                }
                Text(L("Il file viene scritto così com'è: le opzioni qui sotto non vengono applicate.", "The file is written as is: the options below are not applied."))
                    .font(.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                FieldIssue(issue: w.answerFileContents == nil ? .answerFileMissing : nil)
            }
            Group {
                WrapToggle(L("Salta i controlli hardware di Windows 11 (TPM, Secure Boot, RAM)", "Bypass Windows 11 hardware checks (TPM, Secure Boot, RAM)"),
                       isOn: $forge.plan.windows.bypassHardwareChecks)
                WrapToggle(L("Account locale (senza account Microsoft)", "Local account (no Microsoft account)"), isOn: $forge.plan.windows.localAccount)
                if w.localAccount {
                    TextField(L("Nome", "Name"), text: $forge.plan.windows.accountName)
                        .textFieldStyle(.roundedBorder)
                        .accessibilityLabel(L("Nome dell'account locale", "Local account name"))
                    SecureField(L("Password (facoltativa)", "Password (optional)"), text: $forge.plan.windows.accountPassword)
                        .textFieldStyle(.roundedBorder)
                        .accessibilityLabel(L("Password dell'account locale", "Local account password"))
                    FieldIssue(issue: ForgeValidation.windowsAccountIssue(w.accountName) ?? (w.accountPassword.count > 127 ? .windowsPasswordTooLong : nil))
                    let clean = ForgeValidation.windowsAccountName(w.accountName)
                    if !w.accountName.isEmpty && clean != w.accountName.trimmingCharacters(in: .whitespaces) {
                        Text(L("Verrà usato «\(clean)»", "“\(clean)” will be used")).font(.caption).foregroundStyle(.orange)
                    }
                }
                WrapToggle(L("Salta le domande sulla privacy", "Skip privacy questions"), isOn: $forge.plan.windows.skipPrivacy)
                HStack(alignment: .top, spacing: 8) {
                    LabeledField(title: L("Lingua", "Locale")) {
                        TextField(L("Chiedi durante l'installazione", "Ask during setup"), text: $forge.plan.windows.locale)
                            .textFieldStyle(.roundedBorder)
                            .accessibilityLabel(L("Lingua di Windows", "Windows locale"))
                    }
                    LabeledField(title: L("Fuso orario", "Time zone")) {
                        TextField(TimeZone.current.identifier, text: $forge.plan.windows.timeZone)
                            .textFieldStyle(.roundedBorder)
                            .accessibilityLabel(L("Fuso orario di Windows", "Windows time zone"))
                    }
                }
                FieldIssue(issue: w.locale.isEmpty || ForgeValidation.isValidWindowsLocale(w.locale.trimmingCharacters(in: .whitespaces)) ? nil : .windowsLocaleInvalid)
                Button(L("Usa lingua e fuso orario di questo Mac", "Use this Mac's locale and time zone")) {
                    forge.plan.windows.locale = Locale.current.identifier(.bcp47)
                    forge.plan.windows.timeZone = TimeZone.current.identifier
                }
                .buttonStyle(.link)
                .font(.caption)
            }
            .disabled(w.useAnswerFile)
            LabeledField(title: L("Nome del volume", "Volume label")) {
                TextField("ESD-USB", text: $forge.plan.windows.label)
                    .textFieldStyle(.roundedBorder)
                    .accessibilityLabel(L("Nome del volume Windows", "Windows volume label"))
            }
        }
    }

    private var recommendedLayoutSuffix: String {
        guard let layout = model.forge.inspection?.windows?.recommendedLayout else { return "" }
        return " (" + layout.title + ")"
    }

    private func pickAnswerFile() {
        let panel = NSOpenPanel()
        panel.title = L("Scegli autounattend.xml", "Choose autounattend.xml")
        panel.allowedContentTypes = [.xml]
        panel.allowsMultipleSelection = false
        NSApp.activate()
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let contents = try? String(contentsOf: url, encoding: .utf8)
        model.forge.plan.windows.answerFileName = url.lastPathComponent
        model.forge.plan.windows.answerFileContents = contents
        if contents == nil {
            model.forge.startErrors[.create] = L("Impossibile leggere \(url.lastPathComponent) come testo UTF-8.", "Could not read \(url.lastPathComponent) as UTF-8 text.")
        }
    }
}

private struct LinuxSection: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var forge = model.forge
        let linux = forge.plan.linux
        let range = LinuxDraft.persistenceRange(diskSize: forge.targetDisk?.size, imageSize: forge.inspection?.imageSize)
        VStack(alignment: .leading, spacing: 12) {
            if let info = forge.inspection?.linux {
                Text(info.persistence.map { L("Persistenza supportata (\($0)).", "Persistence supported (\($0)).") }
                     ?? L("Questa distribuzione non dichiara un metodo di persistenza noto.", "This distribution doesn't declare a known persistence method."))
                    .font(.caption).foregroundStyle(.secondary)
            }
            WrapToggle(L("Spazio persistente (le modifiche restano dopo il riavvio)", "Persistent storage (changes survive a reboot)"), isOn: $forge.plan.linux.persistence)
                .disabled(range == nil)
            if let range {
                let lo = Double(range.lowerBound), hi = Double(range.upperBound)
                HStack {
                    Slider(value: Binding(get: { min(max(Double(linux.persistenceSize), lo), hi) },
                                          set: { forge.plan.linux.persistenceSize = UInt64($0 / Double(64 << 20)) * (64 << 20) }),
                           in: lo...hi)
                        .accessibilityLabel(L("Dimensione dello spazio persistente", "Persistent storage size"))
                    Text(ByteFormat.string(min(max(linux.persistenceSize, range.lowerBound), range.upperBound)))
                        .font(.callout.monospacedDigit()).frame(minWidth: 70, alignment: .trailing)
                }
                .disabled(!linux.persistence)
            } else {
                Text(L("Scegli un disco con spazio oltre all'immagine per attivarlo.", "Choose a disk with room beyond the image to enable it."))
                    .font(.caption).foregroundStyle(.secondary)
            }
            LabeledField(title: L("Filesystem principale", "Main filesystem")) {
                Picker(L("Filesystem", "Filesystem"), selection: $forge.plan.linux.fs) {
                    Text(L("Automatico (FAT32)", "Automatic (FAT32)")).tag(Choice<ForgeFS>.automatic)
                    ForEach([ForgeFS.fat32, .exfat, .ntfs, .ext4]) { Text($0.displayName).tag(Choice.value($0)) }
                }
                .labelsHidden()
                .accessibilityLabel(L("Filesystem principale", "Main filesystem"))
            }
            LabeledField(title: L("Avvio", "Boot")) {
                Picker(L("Avvio", "Boot"), selection: $forge.plan.linux.boot) {
                    Text(L("Automatico", "Automatic")).tag(Choice<BootTarget>.automatic)
                    ForEach(BootTarget.allCases) { Text($0.title).tag(Choice.value($0)) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .accessibilityLabel(L("Destinazione di avvio", "Boot target"))
            }
            LabeledField(title: L("Nome del volume", "Volume label")) {
                TextField(L("Come l'ISO", "Same as the ISO"), text: $forge.plan.linux.label)
                    .textFieldStyle(.roundedBorder)
                    .accessibilityLabel(L("Nome del volume Linux", "Linux volume label"))
            }
        }
    }
}

private struct RaspberrySection: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var forge = model.forge
        let r = forge.plan.rpi
        VStack(alignment: .leading, spacing: 12) {
            WrapToggle(L("Personalizza il primo avvio", "Customise the first boot"), isOn: $forge.plan.rpi.enabled)
            Group {
                LabeledField(title: L("Nome host", "Hostname")) {
                    TextField("raspberrypi", text: $forge.plan.rpi.hostname)
                        .textFieldStyle(.roundedBorder)
                        .accessibilityLabel(L("Nome host", "Hostname"))
                    FieldIssue(issue: r.hostname.isEmpty || ForgeValidation.isValidHostname(r.hostname.trimmingCharacters(in: .whitespaces)) ? nil : .hostnameInvalid)
                }
                HStack(alignment: .top, spacing: 8) {
                    LabeledField(title: L("Utente", "User")) {
                        TextField("pi", text: $forge.plan.rpi.user)
                            .textFieldStyle(.roundedBorder)
                            .accessibilityLabel(L("Nome utente", "User name"))
                    }
                    LabeledField(title: "Password") {
                        SecureField("", text: $forge.plan.rpi.password)
                            .textFieldStyle(.roundedBorder)
                            .accessibilityLabel(L("Password dell'utente", "User password"))
                    }
                }
                FieldIssue(issue: r.user.isEmpty || ForgeValidation.isValidLinuxUser(r.user.trimmingCharacters(in: .whitespaces)) ? nil : .linuxUserInvalid)
                WrapToggle(L("Attiva SSH", "Enable SSH"), isOn: $forge.plan.rpi.ssh)
                if r.ssh {
                    LabeledField(title: L("Chiavi pubbliche autorizzate (una per riga; vuoto = accesso con password)", "Authorized public keys (one per line; empty = password login)")) {
                        TextEditor(text: $forge.plan.rpi.sshKeys)
                            .font(.system(.caption, design: .monospaced))
                            .frame(height: 54)
                            .scrollContentBackground(.hidden)
                            .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 6))
                            .accessibilityLabel(L("Chiavi SSH autorizzate", "Authorized SSH keys"))
                        FieldIssue(issue: r.sshKeys.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || ForgeValidation.areValidSSHKeys(r.sshKeys) ? nil : .sshKeysInvalid)
                    }
                }
                WrapToggle(L("Configura il Wi-Fi", "Configure Wi-Fi"), isOn: $forge.plan.rpi.wifi)
                if r.wifi {
                    TextField(L("Nome della rete (SSID)", "Network name (SSID)"), text: $forge.plan.rpi.ssid)
                        .textFieldStyle(.roundedBorder)
                        .accessibilityLabel("SSID")
                    SecureField(L("Password Wi-Fi", "Wi-Fi password"), text: $forge.plan.rpi.wifiPassword)
                        .textFieldStyle(.roundedBorder)
                        .accessibilityLabel(L("Password Wi-Fi", "Wi-Fi password"))
                    HStack {
                        TextField(L("Paese (es. IT)", "Country (e.g. IT)"), text: $forge.plan.rpi.country)
                            .textFieldStyle(.roundedBorder)
                            .frame(width: 120)
                            .accessibilityLabel(L("Paese del Wi-Fi", "Wi-Fi country"))
                        WrapToggle(L("Rete nascosta", "Hidden network"), isOn: $forge.plan.rpi.hiddenNetwork)
                    }
                    ForEach(r.issues().filter { [.wifiSSIDInvalid, .wifiCountryRequired, .wifiCountryInvalid, .wifiPasswordInvalid].contains($0) }, id: \.self) {
                        FieldIssue(issue: $0)
                    }
                }
                HStack(alignment: .top, spacing: 8) {
                    LabeledField(title: L("Lingua", "Locale")) {
                        TextField("it_IT.UTF-8", text: $forge.plan.rpi.locale).textFieldStyle(.roundedBorder)
                            .accessibilityLabel(L("Lingua", "Locale"))
                    }
                    LabeledField(title: L("Fuso orario", "Time zone")) {
                        TextField(TimeZone.current.identifier, text: $forge.plan.rpi.timeZone).textFieldStyle(.roundedBorder)
                            .accessibilityLabel(L("Fuso orario", "Time zone"))
                    }
                    LabeledField(title: L("Tastiera", "Keyboard")) {
                        TextField("it", text: $forge.plan.rpi.keyboard).textFieldStyle(.roundedBorder).frame(width: 54)
                            .accessibilityLabel(L("Layout di tastiera", "Keyboard layout"))
                    }
                }
            }
            .disabled(!r.enabled)
        }
    }
}
