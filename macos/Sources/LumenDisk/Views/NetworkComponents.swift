import AppKit
import CoreImage
import CoreImage.CIFilterBuiltins
import SwiftUI

// MARK: - Network health (Feature 5)

/// Plain-language connection state of a remote volume.
struct NetworkHealth: Equatable {
    enum State: Equatable { case fast, slow, connected, syncing, offline }

    var state: State
    var latency: UInt32?
    var pending: UInt64
    var hasQueue: Bool

    static let slowThreshold: UInt32 = 80

    init(volume: VolumeStatus) {
        let remote = volume.remote ?? RemoteStatus()
        latency = remote.latencyMs
        hasQueue = volume.hasSyncQueue
        pending = hasQueue ? remote.pendingOps : 0
        if !remote.online {
            state = .offline
        } else if pending > 0 {
            state = .syncing
        } else if let latency = remote.latencyMs {
            state = latency > Self.slowThreshold ? .slow : .fast
        } else {
            state = .connected
        }
    }

    var tint: Color {
        switch state {
        case .fast, .connected: .ldGood
        case .slow: .ldAmber
        case .syncing: .ldIndigo
        case .offline: .ldPlum
        }
    }

    var symbol: String {
        switch state {
        case .fast, .slow, .connected: "circle.fill"
        case .syncing: "arrow.triangle.2.circlepath"
        case .offline: "icloud.slash.fill"
        }
    }

    private var latencyText: String { latency.map { $0 == 0 ? "< 1 ms" : "\($0) ms" } ?? "" }

    private var waitingFiles: String {
        L10n.plural(pending,
                    one: ("%d file in attesa di sincronizzazione", "%d file waiting to sync"),
                    other: ("%d file in attesa di sincronizzazione", "%d files waiting to sync"))
    }

    /// Headline (bold) and detail of the full pill.
    var headline: String {
        switch state {
        case .fast: L("Connessione rapida", "Fast connection")
        case .slow: L("Connessione lenta", "Slow connection")
        case .connected: L("Connesso", "Connected")
        case .syncing: L("Sincronizzazione", "Syncing")
        case .offline: L("Modalità Offline Attiva", "Offline mode")
        }
    }

    var detail: String {
        switch state {
        case .fast, .slow: return latencyText
        case .connected: return ""
        case .syncing: return waitingFiles
        case .offline:
            if !hasQueue {
                return L("I dati già letti restano disponibili", "Data already read stays available")
            }
            if pending == 0 {
                return L("Nessun blocco del Finder", "Finder never blocks")
            }
            return waitingFiles + L(" (Nessun blocco del Finder)", " (Finder never blocks)")
        }
    }

    /// Short form for sidebar rows and the menu bar.
    var compactText: String {
        switch state {
        case .fast, .slow: return latencyText
        case .connected: return L("Connesso", "Connected")
        case .syncing: return L10n.plural(pending, one: ("%d in attesa", "%d waiting"), other: ("%d in attesa", "%d waiting"))
        case .offline:
            return pending > 0
                ? "Offline · " + L10n.plural(pending, one: ("%d in attesa", "%d waiting"), other: ("%d in attesa", "%d waiting"))
                : "Offline"
        }
    }
}

struct HealthPill: View {
    var health: NetworkHealth
    var compact = false

    var body: some View {
        HStack(spacing: compact ? 4 : 6) {
            Image(systemName: health.symbol)
                .font(.system(size: health.symbol == "circle.fill" ? (compact ? 6 : 7) : (compact ? 9 : 11), weight: .semibold))
                .shadow(color: health.symbol == "circle.fill" ? health.tint.opacity(0.6) : .clear, radius: 2.5)
            if compact {
                Text(health.compactText)
                    .font(.system(size: 10.5, weight: .medium, design: .rounded))
                    .monospacedDigit()
            } else {
                (Text(health.headline).fontWeight(.semibold)
                 + Text(health.detail.isEmpty ? "" : " · " + health.detail))
                    .font(.callout)
                    .monospacedDigit()
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .foregroundStyle(health.tint)
        .padding(.horizontal, compact ? 7 : 11)
        .padding(.vertical, compact ? 2.5 : 5)
        .background(health.tint.opacity(compact ? 0.12 : 0.11), in: Capsule())
        .contentTransition(.numericText())
        .animation(.snappy, value: health)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(health.headline + (health.detail.isEmpty ? "" : ", " + health.detail))
    }
}

// MARK: - Technical details (the only place for SF Mono)

struct TechnicalRow: Identifiable {
    var title: String
    var value: String
    var id: String { title }
}

/// Collapsible "Dettagli tecnici" card.
struct TechnicalDetailsCard: View {
    var rows: [TechnicalRow]
    @State private var expanded = TechnicalDetailsCard.startExpanded

    /// Debug hook (LUMENDISK_TECH=1) for screenshots.
    static var startExpanded = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Button {
                withAnimation(.snappy) { expanded.toggle() }
            } label: {
                HStack(spacing: 10) {
                    Image(systemName: "wrench.and.screwdriver")
                        .symbolRenderingMode(.hierarchical)
                        .foregroundStyle(.secondary)
                        .frame(width: 18)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(L("Dettagli tecnici", "Technical details")).font(.headline)
                        if !expanded {
                            Text(L("Dispositivo, UUID, porte e contatori interni", "Device, UUID, ports and internal counters"))
                                .font(.callout)
                                .foregroundStyle(.secondary)
                        }
                    }
                    Spacer()
                    Image(systemName: "chevron.right")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(.tertiary)
                        .rotationEffect(.degrees(expanded ? 90 : 0))
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityAddTraits(.isHeader)
            .accessibilityValue(expanded ? L("Espanso", "Expanded") : L("Compresso", "Collapsed"))

            if expanded {
                Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 8) {
                    ForEach(rows) { row in
                        GridRow {
                            Text(row.title)
                                .foregroundStyle(.secondary)
                                .gridColumnAlignment(.trailing)
                            Text(row.value)
                                .font(.system(.callout, design: .monospaced))
                                .textSelection(.enabled)
                                .lineLimit(1)
                                .truncationMode(.middle)
                                .help(row.value)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                        .font(.callout)
                    }
                }
                .padding(.top, 14)
                .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .card()
    }
}

// MARK: - Settings

/// Gear menu shared by the toolbar and the menu bar popover.
struct SettingsMenu: View {
    @Environment(AppModel.self) private var model
    var compact = false

    var body: some View {
        @Bindable var settings = model.settings
        Menu {
            Picker(L("Lingua", "Language"), selection: Binding(
                get: { LanguageStore.shared.language },
                set: { LanguageStore.shared.language = $0 }
            )) {
                ForEach(AppLanguage.allCases) { language in
                    Text(language.nativeName).tag(language)
                }
            }
            .pickerStyle(.inline)
            Divider()
            Toggle(L("Crea alias sulla Scrivania", "Create Desktop alias"), isOn: $settings.createDesktopAlias)
        } label: {
            Label(L("Impostazioni", "Settings"), systemImage: "gearshape")
                .labelStyle(.iconOnly)
                .font(compact ? .system(size: 13, weight: .medium) : .body)
        }
        .menuIndicator(.hidden)
        .help(L("Lingua e impostazioni", "Language and settings"))
        .accessibilityLabel(L("Impostazioni", "Settings"))
    }
}

/// Settings window (⌘,).
struct SettingsView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var settings = model.settings
        Form {
            Picker(L("Lingua", "Language"), selection: Binding(
                get: { LanguageStore.shared.language },
                set: { LanguageStore.shared.language = $0 }
            )) {
                ForEach(AppLanguage.allCases) { Text($0.nativeName).tag($0) }
            }
            Toggle(isOn: $settings.createDesktopAlias) {
                Text(L("Crea alias sulla Scrivania", "Create Desktop alias"))
                Text(L("Un alias del Finder per ogni volume montato, rimosso quando lo espelli o esci da LumenDisk.",
                       "A Finder alias for every mounted volume, removed when you eject it or quit LumenDisk."))
            }
        }
        .formStyle(.grouped)
        .frame(width: 460)
        .fixedSize(horizontal: false, vertical: true)
        .navigationTitle(L("Impostazioni", "Settings"))
    }
}

// MARK: - QR code (Feature 4)

@MainActor
enum QRCodeRenderer {
    private static let context = CIContext(options: [.useSoftwareRenderer: false])

    /// Black-on-white QR code (error correction M) scaled by an integer factor with
    /// nearest-neighbour sampling, so every module stays perfectly crisp.
    static func image(for string: String, moduleSize: CGFloat = 12) -> CGImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(string.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage else { return nil }
        let scaled = output.samplingNearest()
            .transformed(by: CGAffineTransform(scaleX: moduleSize.rounded(), y: moduleSize.rounded()))
        return context.createCGImage(scaled, from: scaled.integral.extent)
    }
}

private extension CIImage {
    var integral: CIImage { cropped(to: extent.integral) }
}

struct QRCodeView: View {
    var text: String
    var side: CGFloat = 220

    var body: some View {
        Group {
            if let image = QRCodeRenderer.image(for: text) {
                Image(decorative: image, scale: 1)
                    .resizable()
                    .interpolation(.none)
                    .antialiased(false)
                    .aspectRatio(1, contentMode: .fit)
            } else {
                Image(systemName: "qrcode")
                    .font(.system(size: side * 0.5, weight: .ultraLight))
                    .foregroundStyle(.secondary)
            }
        }
        .frame(width: side, height: side)
        .padding(14)
        .background(Color.white, in: RoundedRectangle(cornerRadius: 18, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 18, style: .continuous).strokeBorder(Color.black.opacity(0.08), lineWidth: 1))
        .shadow(color: .black.opacity(0.10), radius: 12, y: 4)
        .accessibilityLabel(L("Codice QR per \(text)", "QR code for \(text)"))
    }
}

// MARK: - Codes

/// "482913" → "482 913", in large SF Pro Rounded digits.
struct CodeDigits: View {
    var code: String
    var size: CGFloat = 34

    static func grouped(_ code: String) -> String {
        let digits = code.filter { !$0.isWhitespace }
        if digits.count == 6, digits.allSatisfy(\.isNumber) {
            return String(digits.prefix(3)) + " " + String(digits.suffix(3))
        }
        if digits.count == 8, !digits.contains("-") {   // pairing code "ABCDEFGH" → "ABCD-EFGH"
            return String(digits.prefix(4)) + "-" + String(digits.suffix(4))
        }
        return code
    }

    var body: some View {
        Text(Self.grouped(code))
            .font(.system(size: size, weight: .semibold, design: .rounded))
            .monospacedDigit()
            .tracking(size * 0.08)
            .textSelection(.enabled)
            .accessibilityLabel(code.map(String.init).joined(separator: " "))
    }
}
