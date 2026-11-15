import AppKit
import SwiftUI
import UniformTypeIdentifiers

// MARK: - Palette

extension NSAppearance {
    var isDark: Bool { bestMatch(from: [.darkAqua, .aqua]) == .darkAqua }
}

extension Color {
    /// Main canvas behind cards (System Settings–like soft grey / near-black).
    static let ldCanvas = Color(nsColor: NSColor(name: "ldCanvas") { appearance in
        appearance.isDark ? NSColor(srgbRed: 0.118, green: 0.118, blue: 0.125, alpha: 1)
                          : NSColor(srgbRed: 0.957, green: 0.957, blue: 0.965, alpha: 1)
    })
    /// Card surface.
    static let ldCard = Color(nsColor: NSColor(name: "ldCard") { appearance in
        appearance.isDark ? NSColor(srgbRed: 0.173, green: 0.173, blue: 0.180, alpha: 1)
                          : NSColor.white
    })
    /// Inset surface inside cards (rows, wells).
    static let ldWell = Color(nsColor: NSColor(name: "ldWell") { appearance in
        appearance.isDark ? NSColor(white: 1, alpha: 0.045) : NSColor(white: 0, alpha: 0.032)
    })
    static let ldBorder = Color.primary.opacity(0.08)

    static let ldShadowPending = Color(nsColor: NSColor(name: "ldShadowPending") { appearance in
        appearance.isDark ? NSColor(srgbRed: 1.0, green: 0.68, blue: 0.26, alpha: 1)
                          : NSColor(srgbRed: 0.98, green: 0.58, blue: 0.13, alpha: 1)
    })
    static let ldUsed = Color.accentColor

    /// Soft palette for network features.
    static let ldIndigo = Color(nsColor: NSColor(name: "ldIndigo") { appearance in
        appearance.isDark ? NSColor(srgbRed: 0.53, green: 0.55, blue: 1.0, alpha: 1)
                          : NSColor(srgbRed: 0.35, green: 0.36, blue: 0.86, alpha: 1)
    })
    static let ldPlum = Color(nsColor: NSColor(name: "ldPlum") { appearance in
        appearance.isDark ? NSColor(srgbRed: 0.80, green: 0.52, blue: 0.93, alpha: 1)
                          : NSColor(srgbRed: 0.60, green: 0.30, blue: 0.74, alpha: 1)
    })
    static let ldSlate = Color(nsColor: NSColor(name: "ldSlate") { appearance in
        appearance.isDark ? NSColor(srgbRed: 0.58, green: 0.64, blue: 0.74, alpha: 1)
                          : NSColor(srgbRed: 0.38, green: 0.45, blue: 0.56, alpha: 1)
    })
    static let ldGood = Color(nsColor: NSColor(name: "ldGood") { appearance in
        appearance.isDark ? NSColor(srgbRed: 0.30, green: 0.82, blue: 0.52, alpha: 1)
                          : NSColor(srgbRed: 0.13, green: 0.62, blue: 0.36, alpha: 1)
    })
    static let ldAmber = Color(nsColor: NSColor(name: "ldAmber") { appearance in
        appearance.isDark ? NSColor(srgbRed: 1.0, green: 0.72, blue: 0.30, alpha: 1)
                          : NSColor(srgbRed: 0.85, green: 0.52, blue: 0.05, alpha: 1)
    })
}

extension FilesystemKind {
    /// Refined semantic tints for the filesystem pill and drive badges.
    var tint: Color {
        switch self {
        case .ext2, .ext3, .ext4, .linux: Color(red: 0.95, green: 0.55, blue: 0.20)    // warm amber
        case .ntfs: Color(red: 0.23, green: 0.51, blue: 0.96)                          // azure
        case .fat12, .fat16, .fat32: Color(red: 0.55, green: 0.40, blue: 0.93)         // lavender
        case .exfat: Color(red: 0.13, green: 0.68, blue: 0.66)                         // teal
        case .apfs, .hfs: Color(red: 0.45, green: 0.47, blue: 0.52)
        case .unknown: Color.gray
        }
    }
}

// MARK: - Card

struct CardModifier: ViewModifier {
    var padding: CGFloat = 20
    func body(content: Content) -> some View {
        content
            .padding(padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Color.ldCard, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 16, style: .continuous).strokeBorder(Color.ldBorder, lineWidth: 1))
    }
}

extension View {
    func card(padding: CGFloat = 20) -> some View { modifier(CardModifier(padding: padding)) }
}

struct CardHeader: View {
    var title: String
    var subtitle: String?
    var systemImage: String?

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            if let systemImage {
                Image(systemName: systemImage)
                    .symbolRenderingMode(.hierarchical)
                    .foregroundStyle(.secondary)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.headline)
                if let subtitle {
                    Text(subtitle).font(.subheadline).foregroundStyle(.secondary)
                }
            }
        }
    }
}

// MARK: - Badges

struct FilesystemPill: View {
    var kind: FilesystemKind
    var compact = false

    var body: some View {
        Text(kind.badge)
            .font(.system(size: compact ? 9.5 : 11, weight: .semibold, design: .rounded))
            .tracking(0.3)
            .foregroundStyle(kind.tint)
            .padding(.horizontal, compact ? 5 : 8)
            .padding(.vertical, compact ? 1.5 : 3)
            .background(kind.tint.opacity(0.14), in: Capsule())
            .accessibilityLabel("File system \(kind.badge)")
    }
}

/// Pill for remote volume kinds ("SFTP", "WebDAV", "LAN", "URL").
struct KindPill: View {
    var kind: VolumeKind
    var compact = false

    var body: some View {
        Text(kind.badge)
            .font(.system(size: compact ? 9.5 : 11, weight: .semibold, design: .rounded))
            .tracking(0.3)
            .foregroundStyle(kind.tint)
            .padding(.horizontal, compact ? 5 : 8)
            .padding(.vertical, compact ? 1.5 : 3)
            .background(kind.tint.opacity(0.14), in: Capsule())
            .accessibilityLabel(kind.title)
    }
}

/// Small rounded badge for short values (partition numbers, UID pairs, PIDs). Monospace is
/// reserved to the "Dettagli tecnici" section.
struct TagBadge: View {
    var text: String
    var systemImage: String?

    var body: some View {
        HStack(spacing: 4) {
            if let systemImage {
                Image(systemName: systemImage).font(.system(size: 9, weight: .semibold))
            }
            Text(text).font(.system(size: 11, weight: .medium, design: .rounded)).monospacedDigit()
        }
        .foregroundStyle(.secondary)
        .padding(.horizontal, 7)
        .padding(.vertical, 3)
        .background(Color.ldWell, in: RoundedRectangle(cornerRadius: 6, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 6, style: .continuous).strokeBorder(Color.ldBorder, lineWidth: 1))
    }
}

/// Capsule chip with an icon, used for statuses ("Shadow Mode", "Sola Lettura"…).
struct StatusChip: View {
    var title: String
    var systemImage: String
    var tint: Color = .secondary

    var body: some View {
        Label(title, systemImage: systemImage)
            .font(.caption.weight(.medium))
            .symbolRenderingMode(.hierarchical)
            .foregroundStyle(tint)
            .padding(.horizontal, 9)
            .padding(.vertical, 4)
            .background(tint.opacity(0.12), in: Capsule())
    }
}

/// Rounded-square icon tile in the style of System Settings.
struct IconBadge: View {
    var systemImage: String
    var tint: Color
    var size: CGFloat = 44

    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: size * 0.27, style: .continuous)
                .fill(LinearGradient(colors: [tint.opacity(0.85), tint], startPoint: .top, endPoint: .bottom))
            RoundedRectangle(cornerRadius: size * 0.27, style: .continuous)
                .strokeBorder(Color.white.opacity(0.18), lineWidth: 0.5)
            Image(systemName: systemImage)
                .font(.system(size: size * 0.46, weight: .medium))
                .foregroundStyle(.white)
                .symbolRenderingMode(.hierarchical)
        }
        .frame(width: size, height: size)
        .shadow(color: tint.opacity(0.25), radius: size * 0.08, y: size * 0.04)
        .accessibilityHidden(true)
    }
}

extension VolumeStatus {
    var symbol: String {
        if isRemote { return kind.symbol }
        if isPhysical { return "externaldrive.fill" }
        if sourcePath.lowercased().hasSuffix(".iso") { return "opticaldisc.fill" }
        return "cylinder.split.1x2.fill"
    }

    /// Icon tint: filesystem colour for disks, kind colour for network volumes without one.
    var tint: Color {
        if isRemote && (kind != .imageURL || filesystem == .unknown) { return kind.tint }
        return filesystem.tint
    }
}

extension VolumeKind {
    var tint: Color {
        switch self {
        case .local: .accentColor
        case .imageURL: .ldPlum
        case .sftp: .ldSlate
        case .webdav: .ldIndigo
        case .lan: .teal
        }
    }
}

// MARK: - Capacity

struct CapacityBar: View {
    var total: UInt64
    var used: UInt64
    var shadowPending: UInt64 = 0
    var height: CGFloat = 10
    var tint: Color = .ldUsed

    var body: some View {
        GeometryReader { geo in
            let width = geo.size.width
            let total = Double(max(self.total, 1))
            let usedFraction = min(Double(used) / total, 1)
            let shadowFraction = min(Double(shadowPending) / total, max(0, 1 - usedFraction))
            ZStack(alignment: .leading) {
                Capsule().fill(Color.primary.opacity(0.08))
                HStack(spacing: shadowFraction > 0 ? 1.5 : 0) {
                    Rectangle()
                        .fill(LinearGradient(colors: [tint.opacity(0.85), tint], startPoint: .leading, endPoint: .trailing))
                        .frame(width: max(0, width * usedFraction))
                    if shadowFraction > 0 {
                        Rectangle()
                            .fill(Color.ldShadowPending)
                            .frame(width: max(2, width * shadowFraction))
                    }
                }
                .clipShape(Capsule())
            }
        }
        .frame(height: height)
        .animation(.smooth(duration: 0.4), value: used)
        .animation(.smooth(duration: 0.4), value: shadowPending)
        .accessibilityElement()
        .accessibilityLabel(L("Spazio utilizzato", "Used space"))
        .accessibilityValue(L("\(ByteFormat.string(used)) di \(ByteFormat.string(self.total))", "\(ByteFormat.string(used)) of \(ByteFormat.string(self.total))"))
    }
}

struct LegendItem: View {
    var color: Color
    var title: String
    var value: String

    var body: some View {
        HStack(spacing: 7) {
            Circle().fill(color).frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.caption).foregroundStyle(.secondary)
                Text(value).font(.system(.callout, design: .rounded).weight(.semibold)).monospacedDigit()
            }
        }
    }
}

// MARK: - Rows

/// Toggle row with an icon, a title and a subtitle (System Settings style).
struct FeatureToggleRow<Accessory: View>: View {
    var systemImage: String
    var tint: Color
    var title: String
    var subtitle: String
    @Binding var isOn: Bool
    var disabled = false
    @ViewBuilder var accessory: () -> Accessory

    var body: some View {
        HStack(alignment: .center, spacing: 12) {
            IconBadge(systemImage: systemImage, tint: tint, size: 30)
            VStack(alignment: .leading, spacing: 3) {
                Text(title).font(.body.weight(.medium))
                Text(subtitle).font(.callout).foregroundStyle(.secondary)
                accessory()
            }
            Spacer(minLength: 12)
            Toggle(title, isOn: $isOn)
                .toggleStyle(.switch)
                .labelsHidden()
                .disabled(disabled)
        }
        .padding(.vertical, 4)
    }
}

extension FeatureToggleRow where Accessory == EmptyView {
    init(systemImage: String, tint: Color, title: String, subtitle: String, isOn: Binding<Bool>, disabled: Bool = false) {
        self.init(systemImage: systemImage, tint: tint, title: title, subtitle: subtitle, isOn: isOn, disabled: disabled) { EmptyView() }
    }
}

struct InfoRow: View {
    var title: String
    var value: String

    var body: some View {
        GridRow {
            Text(title)
                .foregroundStyle(.secondary)
                .gridColumnAlignment(.trailing)
            Text(value)
                .font(.callout)
                .textSelection(.enabled)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .font(.callout)
    }
}

/// Gentle diagnostic banner (Dirty-bit & Journal Doctor).
struct DiagnosticBanner: View {
    var title: String
    var message: String
    var actionTitle: String
    var isWorking: Bool
    var action: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 14) {
            Image(systemName: "stethoscope")
                .font(.system(size: 20, weight: .medium))
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(.orange)
                .frame(width: 28)
            VStack(alignment: .leading, spacing: 4) {
                Text(title).font(.body.weight(.semibold))
                Text(message).font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 12)
            Button(action: action) {
                if isWorking {
                    ProgressView().controlSize(.small).padding(.horizontal, 24)
                } else {
                    Text(actionTitle)
                }
            }
            .buttonStyle(.borderedProminent)
            .tint(.orange)
            .controlSize(.regular)
            .disabled(isWorking)
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.orange.opacity(0.09), in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 16, style: .continuous).strokeBorder(Color.orange.opacity(0.22), lineWidth: 1))
    }
}

/// Live Ghost Shield counter capsule.
struct GhostBadge: View {
    var count: UInt64
    var compact = false

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: "shield.lefthalf.filled")
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(Color.accentColor)
            if compact {
                Text("\(count)")
                    .font(.system(.caption, design: .rounded).weight(.semibold))
                    .monospacedDigit()
                    .contentTransition(.numericText(value: Double(count)))
            } else {
                (Text("Ghost Shield: ").fontWeight(.semibold) + Text(GhostBadge.sentence(count)))
                    .font(.caption)
                    .monospacedDigit()
                    .contentTransition(.numericText(value: Double(count)))
            }
        }
        .padding(.horizontal, compact ? 8 : 10)
        .padding(.vertical, compact ? 3 : 5)
        .background(Color.accentColor.opacity(0.10), in: Capsule())
        .animation(.snappy, value: count)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Ghost Shield: " + GhostBadge.sentence(count))
    }

    static func sentence(_ count: UInt64) -> String {
        L10n.plural(count,
                    one: ("%d file di sistema macOS bloccato e virtualizzato", "%d macOS system file blocked and virtualised"),
                    other: ("%d file di sistema macOS bloccati e virtualizzati", "%d macOS system files blocked and virtualised"))
    }
}

/// Canvas container used by every detail screen.
struct DetailCanvas<Content: View>: View {
    @ViewBuilder var content: () -> Content

    /// Debug hook (LUMENDISK_SCROLL=bottom) for screenshots of long pages.
    static var startAtBottom: Bool { DetailCanvasDebug.startAtBottom }

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    content()
                    Color.clear.frame(height: 0).id(DetailCanvasDebug.bottomID)
                }
                .padding(.horizontal, 28)
                .padding(.vertical, 24)
                .frame(maxWidth: 860)
                .frame(maxWidth: .infinity)
            }
            .scrollContentBackground(.hidden)
            .background(Color.ldCanvas)
            .task {
                guard Self.startAtBottom else { return }
                try? await Task.sleep(for: .milliseconds(250))
                proxy.scrollTo(DetailCanvasDebug.bottomID, anchor: .bottom)
            }
        }
    }
}

@MainActor
enum DetailCanvasDebug {
    static var startAtBottom = false
    static let bottomID = "detail-canvas-bottom"
}

/// Filled call-to-action button that stays prominent even when the window is in the
/// background (e.g. "Monta Ora" on a disk that macOS left unmounted).
struct CallToActionButtonStyle: ButtonStyle {
    var tint: Color = .accentColor
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.body.weight(.semibold))
            .foregroundStyle(.white)
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .background(tint.opacity(configuration.isPressed ? 0.8 : 1), in: Capsule())
            .shadow(color: tint.opacity(0.3), radius: 4, y: 2)
            .opacity(isEnabled ? 1 : 0.45)
            .contentShape(Capsule())
    }
}

/// Bottom action bar pinned to the detail canvas.
struct ActionBar<Content: View>: View {
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(spacing: 0) {
            Divider()
            HStack(spacing: 10) { content() }
                .padding(.horizontal, 28)
                .padding(.vertical, 12)
                .frame(maxWidth: 860)
                .frame(maxWidth: .infinity)
        }
        .background(.bar)
    }
}

struct PageHeader: View {
    var title: String
    var subtitle: String
    var systemImage: String
    var tint: Color

    var body: some View {
        HStack(alignment: .center, spacing: 14) {
            IconBadge(systemImage: systemImage, tint: tint, size: 46)
            VStack(alignment: .leading, spacing: 3) {
                Text(title).font(.title2.weight(.semibold))
                Text(subtitle).font(.callout).foregroundStyle(.secondary).lineLimit(2)
            }
        }
        .padding(.bottom, 4)
    }
}

// MARK: - App icons for processes

enum ProcessIcon {
    @MainActor
    static func icon(pid: Int32, executable: String?) -> NSImage {
        if let app = NSRunningApplication(processIdentifier: pid), let icon = app.icon { return icon }
        if let executable {
            if let range = executable.range(of: ".app/") {
                let bundlePath = String(executable[..<range.lowerBound]) + ".app"
                return NSWorkspace.shared.icon(forFile: bundlePath)
            }
            if FileManager.default.fileExists(atPath: executable) {
                return NSWorkspace.shared.icon(forFile: executable)
            }
        }
        return NSWorkspace.shared.icon(for: .unixExecutable)
    }
}

extension Date {
    /// "2 minuti fa" / "2 minutes ago", in the app language.
    var relativeDescription: String {
        // Within a few seconds (or a hair in the future) the formatter says "in 0 seconds".
        if abs(timeIntervalSinceNow) < 10 { return L("adesso", "just now") }
        let formatter = RelativeDateTimeFormatter()
        formatter.locale = L10n.locale
        formatter.unitsStyle = .full
        return formatter.localizedString(for: self, relativeTo: Date())
    }
}
