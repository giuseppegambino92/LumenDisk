import AppKit
import SwiftUI

// MARK: - Shared sheet chrome

private struct SheetHeader: View {
    var systemImage: String
    var tint: Color
    var title: String
    var subtitle: String

    var body: some View {
        HStack(alignment: .top, spacing: 14) {
            IconBadge(systemImage: systemImage, tint: tint, size: 44)
            VStack(alignment: .leading, spacing: 3) {
                Text(title).font(.title3.weight(.semibold))
                Text(subtitle)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
        }
    }
}

/// Inline result banner (success / failure) used by the sheets.
private struct ResultBanner: View {
    enum Style { case success, failure, info }
    var style: Style
    var title: String
    var message: String?

    private var tint: Color {
        switch style {
        case .success: .ldGood
        case .failure: .orange
        case .info: .ldIndigo
        }
    }

    private var symbol: String {
        switch style {
        case .success: "checkmark.circle.fill"
        case .failure: "exclamationmark.triangle.fill"
        case .info: "info.circle.fill"
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: symbol)
                .foregroundStyle(tint)
                .font(.system(size: 15))
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.callout.weight(.semibold))
                if let message, !message.isEmpty {
                    Text(message).font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(tint.opacity(0.09), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(tint.opacity(0.18), lineWidth: 1))
        .transition(.opacity)
    }
}

private struct FormLabel: View {
    var text: String
    var body: some View {
        Text(text)
            .foregroundStyle(.secondary)
            .gridColumnAlignment(.trailing)
    }
}

@MainActor
private func chooseKeyFile() -> String? {
    let panel = NSOpenPanel()
    panel.title = L("Scegli la chiave SSH privata", "Choose the private SSH key")
    panel.prompt = L("Scegli", "Choose")
    panel.canChooseDirectories = false
    panel.allowsMultipleSelection = false
    panel.showsHiddenFiles = true
    panel.directoryURL = URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent(".ssh", isDirectory: true)
    return panel.runModal() == .OK ? panel.url?.path : nil
}

// MARK: - Connect to Server (SFTP / WebDAV)

struct ConnectServerSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    enum Auth: Hashable { case password, key }
    enum TestState: Equatable {
        case idle, testing
        case success(latency: UInt32?, entries: UInt32?)
        case failure(String)
    }

    @State private var kind: RemoteProtocol = .sftp
    @State private var address = ""
    @State private var user = ""
    @State private var auth: Auth = .password
    @State private var password = ""
    @State private var keyPath: String?
    @State private var name = ""
    @State private var mode: MountMode = .readWrite
    @State private var test: TestState = .idle
    @State private var isConnecting = false
    @State private var connectError: String?

    #if DEBUG
    /// Screenshot hook: LUMENDISK_SHEET=connect-filled (with LUMENDISK_CONNECT_URL).
    nonisolated(unsafe) static var debugPrefill = false
    #endif

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            SheetHeader(systemImage: "server.rack", tint: .ldSlate,
                        title: L("Connetti a Server", "Connect to Server"),
                        subtitle: L("Il disco remoto appare nel Finder. Se la rete cade continui a lavorare offline: le modifiche vengono sincronizzate dopo.",
                                    "The remote disk appears in Finder. If the network drops you keep working offline: changes are synced later."))
                .padding(24)
            Divider()
            form.padding(24)
            Divider()
            footer
                .padding(.horizontal, 24)
                .padding(.vertical, 16)
        }
        .frame(width: 560)
        .onChange(of: address) { _, new in
            let lower = new.lowercased()
            if lower.hasPrefix("sftp://") || lower.hasPrefix("ssh://") { kind = .sftp }
            if lower.hasPrefix("http://") || lower.hasPrefix("https://") || lower.hasPrefix("webdav://") { kind = .webdav }
            test = .idle
        }
        .onChange(of: kind) { _, _ in test = .idle }
        .onAppear {
            #if DEBUG
            // Screenshot hook: prefill a real address (LUMENDISK_CONNECT_URL) and run a real test.
            if Self.debugPrefill, let url = ProcessInfo.processInfo.environment["LUMENDISK_CONNECT_URL"], !url.isEmpty {
                address = url
                Task {
                    try? await Task.sleep(for: .milliseconds(200))
                    await runTest()
                }
            }
            #endif
        }
    }

    private var form: some View {
        Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 14) {
            GridRow {
                FormLabel(text: L("Tipo", "Type"))
                Picker(L("Tipo", "Type"), selection: $kind) {
                    Text("SFTP").tag(RemoteProtocol.sftp)
                    Text("WebDAV").tag(RemoteProtocol.webdav)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()
            }
            GridRow {
                FormLabel(text: L("Indirizzo", "Address"))
                TextField(L("Indirizzo", "Address"), text: $address,
                          prompt: Text(kind == .sftp ? "sftp://raspberrypi.local/home/pi" : "https://nas.local/dav/"))
                    .textFieldStyle(.roundedBorder)
                    .labelsHidden()
                    .textContentType(.URL)
            }
            GridRow {
                FormLabel(text: L("Utente", "User"))
                TextField(L("Utente", "User"), text: $user, prompt: Text(kind == .sftp ? "pi" : L("facoltativo", "optional")))
                    .textFieldStyle(.roundedBorder)
                    .labelsHidden()
                    .textContentType(.username)
            }
            if kind == .sftp {
                GridRow {
                    FormLabel(text: L("Accesso", "Sign in with"))
                    Picker(L("Accesso", "Sign in with"), selection: $auth) {
                        Text("Password").tag(Auth.password)
                        Text(L("Chiave SSH", "SSH Key")).tag(Auth.key)
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .fixedSize()
                }
            }
            if kind == .webdav || auth == .password {
                GridRow {
                    FormLabel(text: "Password")
                    SecureField("Password", text: $password,
                                prompt: Text(kind == .sftp ? L("vuota = ssh-agent e chiavi in ~/.ssh", "empty = ssh-agent and keys in ~/.ssh") : L("facoltativa", "optional")))
                        .textFieldStyle(.roundedBorder)
                        .labelsHidden()
                        .textContentType(.password)
                }
            } else {
                GridRow {
                    FormLabel(text: L("Chiave", "Key"))
                    HStack {
                        Image(systemName: "key.fill").foregroundStyle(.secondary)
                        Text(keyPath.map { ($0 as NSString).lastPathComponent } ?? L("Nessuna chiave scelta", "No key chosen"))
                            .foregroundStyle(keyPath == nil ? .secondary : .primary)
                            .lineLimit(1)
                        Spacer()
                        Button(L("Scegli…", "Choose…")) {
                            if let path = chooseKeyFile() { keyPath = path; test = .idle }
                        }
                    }
                    .padding(.horizontal, 10)
                    .padding(.vertical, 6)
                    .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                }
            }
            GridRow {
                FormLabel(text: L("Nome nel Finder", "Name in Finder"))
                TextField(L("Nome nel Finder", "Name in Finder"), text: $name, prompt: Text(suggestedName ?? L("automatico", "automatic")))
                    .textFieldStyle(.roundedBorder)
                    .labelsHidden()
            }
            GridRow {
                FormLabel(text: L("Modalità", "Mode"))
                VStack(alignment: .leading, spacing: 4) {
                    Picker(L("Modalità", "Mode"), selection: $mode) {
                        ForEach(MountMode.allCases) { Text($0.title).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    Text(mode.explanation).font(.caption).foregroundStyle(.secondary)
                }
            }
            if let banner {
                GridRow {
                    Color.clear.frame(width: 1, height: 1)
                    banner
                }
            }
        }
        .animation(.snappy, value: test)
        .animation(.snappy, value: connectError)
    }

    private var banner: ResultBanner? {
        if let connectError {
            return ResultBanner(style: .failure, title: L("Connessione non riuscita", "Couldn’t connect"), message: connectError)
        }
        switch test {
        case .success(let latency, let entries):
            var parts: [String] = []
            if let latency { parts.append(L("risposta in \(latency) ms", "responded in \(latency) ms")) }
            if let entries {
                parts.append(L10n.plural(entries, one: ("%d elemento nella cartella", "%d item in the folder"),
                                         other: ("%d elementi nella cartella", "%d items in the folder")))
            }
            return ResultBanner(style: .success, title: L("Connessione riuscita", "Connection works"),
                                message: parts.joined(separator: " · ").capitalizedFirst)
        case .failure(let message):
            return ResultBanner(style: .failure, title: L("Il server non risponde come previsto", "The server didn’t respond as expected"), message: message)
        default:
            return nil
        }
    }

    private var footer: some View {
        HStack(spacing: 10) {
            Button {
                Task { await runTest() }
            } label: {
                HStack(spacing: 6) {
                    if test == .testing { ProgressView().controlSize(.small) }
                    Text(L("Prova connessione", "Test Connection"))
                }
            }
            .disabled(request == nil || test == .testing || isConnecting)
            Spacer()
            Button(L("Annulla", "Cancel"), role: .cancel) { dismiss() }
                .keyboardShortcut(.cancelAction)
            Button {
                Task { await connect() }
            } label: {
                if isConnecting {
                    HStack(spacing: 6) { ProgressView().controlSize(.small); Text(L("Connessione…", "Connecting…")) }
                } else {
                    Text(L("Connetti", "Connect"))
                }
            }
            .buttonStyle(.borderedProminent)
            .keyboardShortcut(.defaultAction)
            .disabled(request == nil || isConnecting)
        }
        .controlSize(.large)
    }

    // MARK: Logic

    private var normalizedURL: String? {
        var text = address.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return nil }
        if text.lowercased().hasPrefix("webdav://") { text = "https://" + text.dropFirst("webdav://".count) }
        if text.lowercased().hasPrefix("ssh://") { text = "sftp://" + text.dropFirst("ssh://".count) }
        if !text.contains("://") { text = (kind == .sftp ? "sftp://" : "https://") + text }
        guard var components = URLComponents(string: text), let host = components.host, !host.isEmpty else { return nil }
        let trimmedUser = user.trimmingCharacters(in: .whitespaces)
        if kind == .sftp, components.user == nil, !trimmedUser.isEmpty { components.user = trimmedUser }
        return components.string
    }

    private var suggestedName: String? {
        guard let url = normalizedURL.flatMap(URL.init(string:)) else { return nil }
        let last = url.lastPathComponent
        return (last.isEmpty || last == "/") ? url.host() : last
    }

    private var request: RemoteMountRequest? {
        guard let url = normalizedURL else { return nil }
        let trimmedUser = user.trimmingCharacters(in: .whitespaces)
        return RemoteMountRequest(
            kind: kind,
            url: url,
            password: (kind == .webdav || auth == .password) && !password.isEmpty ? password : nil,
            identityFile: kind == .sftp && auth == .key ? keyPath : nil,
            username: kind == .webdav && !trimmedUser.isEmpty ? trimmedUser : nil,
            mode: mode,
            name: name.trimmingCharacters(in: .whitespaces).nonEmpty)
    }

    private func runTest() async {
        guard let request else { return }
        connectError = nil
        test = .testing
        do {
            let result = try await model.probeRemote(request)
            test = .success(latency: result.latencyMs, entries: result.entries)
        } catch {
            test = .failure(error.localizedDescription)
        }
    }

    private func connect() async {
        guard let request else { return }
        isConnecting = true
        connectError = nil
        defer { isConnecting = false }
        do {
            try await model.mountRemote(request)
            dismiss()
        } catch {
            connectError = error.localizedDescription
        }
    }
}

private extension String {
    var capitalizedFirst: String {
        guard let first else { return self }
        return first.uppercased() + dropFirst()
    }
}

// MARK: - Mount Image from URL

struct ImageURLSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    @State private var urlText = ""
    @State private var password = ""
    @State private var keyPath: String?
    @State private var showAuth = false
    @State private var isInspecting = false
    @State private var inspection: ImageURLInspection?
    @State private var inspectedURL: String?
    @State private var error: String?
    @State private var selected: UInt32?
    @State private var mode: MountMode = .shadow
    @State private var isMounting = false

    #if DEBUG
    /// Screenshot hook: LUMENDISK_SHEET=imageurl-filled (with LUMENDISK_IMAGE_URL).
    nonisolated(unsafe) static var debugPrefill = false
    #endif

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            SheetHeader(systemImage: "arrow.down.circle.fill", tint: .ldPlum,
                        title: L("Monta Immagine da URL", "Mount Image from URL"),
                        subtitle: L("LumenDisk legge solo i blocchi che servono, direttamente dal server. Le modifiche restano su questo Mac: il file remoto non viene mai toccato.",
                                    "LumenDisk reads only the blocks it needs, straight from the server. Changes stay on this Mac: the remote file is never touched."))
                .padding(24)
            Divider()
            VStack(alignment: .leading, spacing: 16) {
                urlRow
                authSection
                if let error {
                    ResultBanner(style: .failure, title: L("Impossibile leggere l'immagine", "Couldn’t read the image"), message: error)
                }
                if let inspection { inspectionView(inspection) }
            }
            .padding(24)
            .animation(.snappy, value: inspection)
            .animation(.snappy, value: error)
            Divider()
            footer
                .padding(.horizontal, 24)
                .padding(.vertical, 16)
        }
        .frame(width: 600)
        .onAppear {
            #if DEBUG
            // Screenshot hook: inspect a real image URL (LUMENDISK_IMAGE_URL).
            if Self.debugPrefill, let url = ProcessInfo.processInfo.environment["LUMENDISK_IMAGE_URL"], !url.isEmpty {
                urlText = url
                Task { await inspect() }
            }
            #endif
        }
    }

    private var urlRow: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(L("Indirizzo dell'immagine", "Image address")).font(.callout.weight(.medium))
            HStack(spacing: 8) {
                TextField(L("Indirizzo dell'immagine", "Image address"), text: $urlText,
                          prompt: Text("https://example.com/raspios-lite.img"))
                    .textFieldStyle(.roundedBorder)
                    .labelsHidden()
                    .onSubmit { Task { await inspect() } }
                Button {
                    Task { await inspect() }
                } label: {
                    HStack(spacing: 6) {
                        if isInspecting { ProgressView().controlSize(.small) }
                        Text(L("Analizza", "Analyse"))
                    }
                }
                .disabled(normalizedURL == nil || isInspecting)
            }
            Text(L("HTTP/HTTPS con richieste Range, oppure sftp://. Formati: .img, .raw, .iso, .bin", "HTTP/HTTPS with Range requests, or sftp://. Formats: .img, .raw, .iso, .bin"))
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    private var authSection: some View {
        DisclosureGroup(isExpanded: $showAuth) {
            HStack(spacing: 10) {
                SecureField("Password", text: $password, prompt: Text(L("Password (facoltativa)", "Password (optional)")))
                    .textFieldStyle(.roundedBorder)
                if normalizedURL?.lowercased().hasPrefix("sftp://") == true {
                    Button(keyPath.map { ($0 as NSString).lastPathComponent } ?? L("Chiave SSH…", "SSH Key…")) {
                        if let path = chooseKeyFile() { keyPath = path }
                    }
                }
            }
            .padding(.top, 6)
        } label: {
            Text(L("Il server chiede una password?", "Does the server need a password?"))
                .font(.callout)
        }
    }

    private func inspectionView(_ inspection: ImageURLInspection) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 12) {
                Image(systemName: "cylinder.split.1x2.fill")
                    .font(.system(size: 22))
                    .symbolRenderingMode(.hierarchical)
                    .foregroundStyle(Color.ldPlum)
                    .frame(width: 32)
                VStack(alignment: .leading, spacing: 2) {
                    Text(fileName(inspection.path)).font(.body.weight(.medium)).lineLimit(1).truncationMode(.middle)
                    Text(summary(inspection))
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
            VStack(spacing: 0) {
                ForEach(Array(inspection.partitions.enumerated()), id: \.element.id) { index, partition in
                    if index > 0 { Divider().padding(.leading, 76) }
                    partitionRow(partition)
                }
            }
            .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 12, style: .continuous))

            VStack(alignment: .leading, spacing: 6) {
                Picker(L("Modalità", "Mode"), selection: $mode) {
                    Text(MountMode.shadow.title).tag(MountMode.shadow)
                    Text(MountMode.readOnly.title).tag(MountMode.readOnly)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()
                Text(mode == .shadow
                     ? L("Puoi modificare i file: le modifiche restano su questo Mac, l'immagine remota non cambia mai.",
                         "You can edit files: changes stay on this Mac and the remote image never changes.")
                     : L("I file si possono solo leggere.", "Files can only be read."))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func partitionRow(_ partition: PartitionInfo) -> some View {
        let isSelected = selected == partition.index
        return Button {
            if partition.isMountable { selected = partition.index }
        } label: {
            HStack(spacing: 12) {
                Image(systemName: isSelected ? "largecircle.fill.circle" : "circle")
                    .font(.system(size: 16))
                    .foregroundStyle(isSelected ? Color.accentColor : Color.secondary)
                IconBadge(systemImage: partition.isMountable ? "internaldrive.fill" : "questionmark.square.dashed",
                          tint: partition.isMountable ? partition.filesystem.tint : .gray, size: 30)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(partition.displayName).font(.body.weight(.medium))
                        if partition.fsType != nil { FilesystemPill(kind: partition.filesystem, compact: true) }
                    }
                    Text((partition.index == 0 ? L("Volume intero", "Whole volume") : L("Partizione \(partition.index)", "Partition \(partition.index)"))
                         + (partition.typeName.isEmpty ? "" : " · \(partition.typeName)"))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Text(ByteFormat.string(partition.size))
                    .font(.system(.callout, design: .rounded))
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
            .contentShape(Rectangle())
            .opacity(partition.isMountable ? 1 : 0.55)
        }
        .buttonStyle(.plain)
        .disabled(!partition.isMountable)
        .accessibilityAddTraits(isSelected ? .isSelected : [])
    }

    private var footer: some View {
        HStack(spacing: 10) {
            Label(L("Nessun download completo", "No full download"), systemImage: "bolt.horizontal.circle")
                .font(.callout)
                .foregroundStyle(.secondary)
            Spacer()
            Button(L("Annulla", "Cancel"), role: .cancel) { dismiss() }
                .keyboardShortcut(.cancelAction)
            Button {
                Task { await mount() }
            } label: {
                if isMounting {
                    HStack(spacing: 6) { ProgressView().controlSize(.small); Text(L("Montaggio…", "Mounting…")) }
                } else {
                    Text(L("Monta", "Mount"))
                }
            }
            .buttonStyle(.borderedProminent)
            .keyboardShortcut(.defaultAction)
            .disabled(selected == nil || inspection == nil || isMounting || inspectedURL != normalizedURL)
        }
        .controlSize(.large)
    }

    // MARK: Logic

    private var normalizedURL: String? {
        var text = urlText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return nil }
        if !text.contains("://") { text = "https://" + text }
        guard let url = URL(string: text), let scheme = url.scheme?.lowercased(),
              ["http", "https", "sftp"].contains(scheme), url.host() != nil else { return nil }
        return text
    }

    private func request(partition: UInt32) -> ImageURLRequest? {
        guard let url = normalizedURL else { return nil }
        return ImageURLRequest(url: url, password: password.isEmpty ? nil : password, identityFile: keyPath,
                               partition: partition, mode: mode, name: nil)
    }

    private func inspect() async {
        guard let request = request(partition: 0), !isInspecting else { return }
        isInspecting = true
        error = nil
        defer { isInspecting = false }
        do {
            let result = try await model.inspectImageURL(request)
            inspection = result
            inspectedURL = request.url
            selected = result.partitions.first(where: { $0.isMountable && $0.filesystem != .fat32 && $0.filesystem != .fat16 })?.index
                ?? result.partitions.first(where: \.isMountable)?.index
        } catch {
            inspection = nil
            self.error = error.localizedDescription
        }
    }

    private func mount() async {
        guard let selected, var request = request(partition: selected) else { return }
        if let partition = inspection?.partitions.first(where: { $0.index == selected }), let label = partition.label, !label.isEmpty {
            request.name = label
        }
        isMounting = true
        error = nil
        defer { isMounting = false }
        do {
            try await model.mountImageURL(request)
            dismiss()
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func fileName(_ path: String) -> String {
        URL(string: path)?.lastPathComponent.nonEmpty ?? path
    }

    private func summary(_ inspection: ImageURLInspection) -> String {
        var parts = [ByteFormat.string(inspection.size)]
        let scheme = inspection.partitions.first?.scheme ?? "NONE"
        parts.append(scheme == "NONE" ? L("Senza tabella partizioni", "No partition table") : L("Tabella \(scheme)", "\(scheme) table"))
        if inspection.fetchedBytes > 0 {
            parts.append(L("analizzata leggendo solo \(ByteFormat.string(inspection.fetchedBytes))",
                           "analysed by reading just \(ByteFormat.string(inspection.fetchedBytes))"))
        }
        return parts.joined(separator: " · ")
    }
}

// MARK: - LAN pairing

struct PairingSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    var peer: LANPeer

    @State private var code = ""
    @State private var mode: MountMode = .readWrite
    @State private var isConnecting = false
    @State private var error: String?
    @FocusState private var focused: Bool

    /// Pairing codes are 8 characters from an unambiguous alphabet, shown as "ABCD-EFGH".
    static let codeLength = 8
    static func normalise(_ text: String) -> String {
        String(text.uppercased().filter { $0.isASCII && ($0.isLetter || $0.isNumber) }.prefix(codeLength))
    }

    private var digits: String { Self.normalise(code) }

    var body: some View {
        VStack(spacing: 0) {
            SheetHeader(systemImage: "desktopcomputer", tint: .teal,
                        title: L("Connetti a «\(peer.name)»", "Connect to “\(peer.name)”"),
                        subtitle: L("Inserisci il codice di abbinamento mostrato sul Mac che condivide il disco (es. ABCD-EFGH).",
                                    "Enter the pairing code shown on the Mac that shares the disk (e.g. ABCD-EFGH)."))
                .padding(24)
            Divider()
            VStack(spacing: 18) {
                codeBoxes
                VStack(spacing: 6) {
                    Picker(L("Modalità", "Mode"), selection: $mode) {
                        ForEach(MountMode.allCases) { Text($0.title).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .fixedSize()
                    Text(mode.explanation).font(.caption).foregroundStyle(.secondary)
                }
                if let error {
                    ResultBanner(style: .failure, title: L("Connessione non riuscita", "Couldn’t connect"), message: error)
                }
            }
            .padding(24)
            Divider()
            HStack {
                Spacer()
                Button(L("Annulla", "Cancel"), role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button {
                    Task { await connect() }
                } label: {
                    if isConnecting {
                        HStack(spacing: 6) { ProgressView().controlSize(.small); Text(L("Connessione…", "Connecting…")) }
                    } else {
                        Text(L("Connetti", "Connect"))
                    }
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
                .disabled(digits.count != Self.codeLength || isConnecting)
            }
            .controlSize(.large)
            .padding(.horizontal, 24)
            .padding(.vertical, 16)
        }
        .frame(width: 480)
        .onAppear { focused = true }
    }

    /// Six rounded digit boxes over an invisible text field.
    private var codeBoxes: some View {
        ZStack {
            TextField("", text: $code)
                .focused($focused)
                .textFieldStyle(.plain)
                .foregroundStyle(.clear)
                .tint(.clear)
                .frame(width: 1, height: 1)
                .opacity(0.01)
                .onChange(of: code) { _, new in
                    let filtered = Self.normalise(new)
                    if filtered != new { code = filtered }
                    error = nil
                }
                .accessibilityLabel(L("Codice di abbinamento", "Pairing code"))
            HStack(spacing: 7) {
                ForEach(0..<Self.codeLength, id: \.self) { index in
                    let characters = Array(digits)
                    let char = index < characters.count ? String(characters[index]) : ""
                    let isCurrent = focused && index == min(characters.count, Self.codeLength - 1)
                    Text(char)
                        .font(.system(size: 26, weight: .semibold, design: .rounded))
                        .monospacedDigit()
                        .frame(width: 40, height: 52)
                        .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                        .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous)
                            .strokeBorder(isCurrent ? Color.accentColor : Color.ldBorder, lineWidth: isCurrent ? 2 : 1))
                    if index == Self.codeLength / 2 - 1 {
                        Text("–").font(.title2.weight(.semibold)).foregroundStyle(.tertiary)
                    }
                }
            }
            .contentShape(Rectangle())
            .onTapGesture { focused = true }
            .accessibilityHidden(true)
        }
    }

    private func connect() async {
        isConnecting = true
        error = nil
        defer { isConnecting = false }
        do {
            let code = String(digits.prefix(Self.codeLength / 2)) + "-" + String(digits.suffix(Self.codeLength / 2))
            try await model.connect(to: peer, pairingCode: code, mode: mode)
            dismiss()
        } catch {
            self.error = error.localizedDescription
        }
    }
}

// MARK: - Wi-Fi portal (QR)

struct PortalSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    var volumeID: String

    @State private var isStopping = false
    @State private var copied = false
    @State private var confirmingPlainHTTP = false
    @State private var switchingPlainHTTP = false

    var body: some View {
        let volume = model.volume(id: volumeID)
        let portal = volume.flatMap { model.portal(for: $0) }
        VStack(spacing: 0) {
            SheetHeader(systemImage: "qrcode", tint: .ldPlum,
                        title: L("Ricevi/Invia via Wi-Fi", "Send & Receive over Wi-Fi"),
                        subtitle: L("Inquadra il codice con la fotocamera di un telefono collegato alla stessa rete Wi-Fi: si apre una pagina per caricare e scaricare file su «\(volume?.displayName ?? "")».",
                                    "Scan the code with the camera of a phone on the same Wi-Fi network: a page opens to upload and download files on “\(volume?.displayName ?? "")”."))
                .padding(24)
            Divider()
            if let portal {
                content(portal)
                    .padding(24)
            } else {
                ContentUnavailableView(L("Portale non attivo", "Portal not running"), systemImage: "qrcode",
                                       description: Text(L("Il portale Wi-Fi è stato interrotto o il volume è stato espulso.",
                                                           "The Wi-Fi portal was stopped or the volume was ejected.")))
                    .frame(height: 300)
            }
            Divider()
            HStack {
                if portal != nil {
                    Button(role: .destructive) {
                        isStopping = true
                        Task {
                            await model.stopPortal(volumeID)
                            isStopping = false
                            dismiss()
                        }
                    } label: {
                        HStack(spacing: 6) {
                            if isStopping { ProgressView().controlSize(.small) } else { Image(systemName: "stop.circle") }
                            Text(L("Interrompi", "Stop"))
                        }
                    }
                    .disabled(isStopping)
                }
                Spacer()
                Button(L("Fine", "Done")) { dismiss() }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
            }
            .controlSize(.large)
            .padding(.horizontal, 24)
            .padding(.vertical, 16)
        }
        .frame(width: 500)
    }

    /// The QR shows the unencrypted address only while the user has turned it on.
    private func shownURL(_ portal: PortalStatus) -> String {
        portal.compatibilityURL ?? portal.url
    }

    private func setPlainHTTP(_ enabled: Bool) {
        switchingPlainHTTP = true
        copied = false
        Task {
            await model.setPortalPlainHTTP(volumeID, enabled: enabled)
            switchingPlainHTTP = false
        }
    }

    private func content(_ portal: PortalStatus) -> some View {
        let url = shownURL(portal)
        let encrypted = url.lowercased().hasPrefix("https://")
        return VStack(spacing: 18) {
            QRCodeView(text: url, side: 216)
                .id(url)
                .transition(.opacity)
            VStack(spacing: 8) {
                urlCapsule(url, encrypted: encrypted)
                compatibilityToggle(insecureOn: portal.compatibilityURL != nil)
            }
            .animation(.snappy, value: encrypted)

            HStack(spacing: 10) {
                VStack(spacing: 2) {
                    Text("PIN").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                    CodeDigits(code: portal.pin, size: 28)
                }
                .frame(maxWidth: .infinity)
                .padding(.vertical, 12)
                .background(Color.ldPlum.opacity(0.10), in: RoundedRectangle(cornerRadius: 14, style: .continuous))
                counter(title: L("Ricevuti", "Received"), value: portal.uploads, symbol: "tray.and.arrow.down.fill", tint: .ldGood)
                counter(title: L("Inviati", "Sent"), value: portal.downloads, symbol: "tray.and.arrow.up.fill", tint: .ldIndigo)
            }
            Text(L("Il portale resta attivo finché non lo interrompi o espelli il volume. Chi non conosce il PIN non può entrare.",
                   "The portal stays on until you stop it or eject the volume. Nobody can get in without the PIN."))
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func urlCapsule(_ url: String, encrypted: Bool) -> some View {
        HStack(spacing: 8) {
            Image(systemName: encrypted ? "lock.fill" : "lock.open")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(encrypted ? Color.ldGood : Color.secondary)
                .help(encrypted ? L("Connessione cifrata (HTTPS)", "Encrypted connection (HTTPS)")
                                : L("Indirizzo non cifrato", "Unencrypted address"))
            Text(url)
                .font(.system(.callout, design: .rounded).weight(.medium))
                .textSelection(.enabled)
                .lineLimit(1)
                .truncationMode(.middle)
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(url, forType: .string)
                copied = true
            } label: {
                Image(systemName: copied ? "checkmark" : "doc.on.doc")
                    .contentTransition(.symbolEffect(.replace))
            }
            .buttonStyle(.borderless)
            .help(L("Copia link", "Copy link"))
            .accessibilityLabel(L("Copia link", "Copy link"))
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(Color.ldWell, in: Capsule())
    }

    /// "Il telefono mostra un avviso?": the unencrypted address is an explicit, confirmed opt-in
    /// (the core doesn't even listen on HTTP until then) and can be switched off again.
    @ViewBuilder
    private func compatibilityToggle(insecureOn: Bool) -> some View {
        if insecureOn {
            VStack(spacing: 4) {
                Label(L("Modalità non sicura attiva: PIN e file viaggiano in chiaro su questa rete. Disattivala appena hai finito.",
                        "Insecure mode is on: the PIN and files travel in clear on this network. Turn it off as soon as you’re done."),
                      systemImage: "exclamationmark.shield.fill")
                    .font(.caption)
                    .foregroundStyle(Color.ldAmber)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
                Button(L("Disattiva l'indirizzo non cifrato", "Turn off the unencrypted address")) { setPlainHTTP(false) }
                    .buttonStyle(.link)
                    .font(.caption)
                    .disabled(switchingPlainHTTP)
            }
        } else {
            Button(L("Il telefono non riesce ad aprire la pagina?", "Phone can’t open the page?")) {
                confirmingPlainHTTP = true
            }
            .buttonStyle(.link)
            .font(.caption)
            .disabled(switchingPlainHTTP)
            .help(L("Il portale usa un certificato creato da questo Mac: alcuni telefoni mostrano un avviso la prima volta. Di solito basta accettarlo.",
                    "The portal uses a certificate made by this Mac: some phones show a warning the first time. Accepting it is usually enough."))
            .alert(L("Attivare un indirizzo non cifrato?", "Turn on an unencrypted address?"), isPresented: $confirmingPlainHTTP) {
                Button(L("Attiva modalità non sicura", "Turn On Insecure Mode"), role: .destructive) { setPlainHTTP(true) }
                Button(L("Annulla", "Cancel"), role: .cancel) {}
            } message: {
                Text(L("Prima prova ad accettare l'avviso del telefono sull'indirizzo cifrato: è sicuro, il certificato è di questo Mac.\n\nSull'indirizzo http:// il PIN, l'elenco dei file e i file trasferiti viaggiano in chiaro: chiunque sia sulla stessa rete può leggerli o modificarli. Usalo solo su una rete di cui ti fidi e disattivalo appena hai finito.",
                       "First try accepting the phone’s warning on the encrypted address: it’s safe, the certificate belongs to this Mac.\n\nOn the http:// address the PIN, the file list and the transferred files travel in clear: anyone on the same network can read or change them. Only use it on a network you trust and turn it off as soon as you’re done."))
            }
        }
    }

    private func counter(title: String, value: UInt64, symbol: String, tint: Color) -> some View {
        VStack(spacing: 4) {
            Label(title, systemImage: symbol)
                .font(.caption.weight(.semibold))
                .foregroundStyle(tint)
            Text("\(value)")
                .font(.system(size: 26, weight: .semibold, design: .rounded))
                .monospacedDigit()
                .contentTransition(.numericText(value: Double(value)))
                .animation(.snappy, value: value)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 12)
        .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
    }
}
