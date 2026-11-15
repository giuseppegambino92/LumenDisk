import Foundation
import Observation
import os

/// Key-value preferences. Normally backed by `UserDefaults.standard`; when the environment
/// variable `LUMENDISK_SUPPORT_DIR` is set (automated launches) they live in
/// `$LUMENDISK_SUPPORT_DIR/preferences.json` instead, so tests leave nothing behind.
final class Preferences: Sendable {
    static let shared = Preferences()

    private let fileURL: URL?
    private let values: OSAllocatedUnfairLock<[String: PreferenceValue]>

    private enum PreferenceValue: Codable, Sendable {
        case string(String)
        case bool(Bool)
        case strings([String])
    }

    private init() {
        if AppPaths.isIsolated {
            let url = AppPaths.supportDirectory.appendingPathComponent("preferences.json")
            fileURL = url
            let stored = (try? Data(contentsOf: url)).flatMap { try? JSONDecoder().decode([String: PreferenceValue].self, from: $0) }
            values = OSAllocatedUnfairLock(initialState: stored ?? [:])
        } else {
            fileURL = nil
            values = OSAllocatedUnfairLock(initialState: [:])
        }
    }

    func string(forKey key: String) -> String? {
        guard fileURL != nil else { return UserDefaults.standard.string(forKey: key) }
        if case .string(let v)? = values.withLock({ $0[key] }) { return v }
        return nil
    }

    func bool(forKey key: String) -> Bool {
        guard fileURL != nil else { return UserDefaults.standard.bool(forKey: key) }
        if case .bool(let v)? = values.withLock({ $0[key] }) { return v }
        return false
    }

    func strings(forKey key: String) -> [String] {
        guard fileURL != nil else { return UserDefaults.standard.stringArray(forKey: key) ?? [] }
        if case .strings(let v)? = values.withLock({ $0[key] }) { return v }
        return []
    }

    func set(_ value: String, forKey key: String) { store(.string(value), key) }
    func set(_ value: Bool, forKey key: String) { store(.bool(value), key) }
    func set(_ value: [String], forKey key: String) { store(.strings(value), key) }

    private func store(_ value: PreferenceValue, _ key: String) {
        guard let fileURL else {
            switch value {
            case .string(let v): UserDefaults.standard.set(v, forKey: key)
            case .bool(let v): UserDefaults.standard.set(v, forKey: key)
            case .strings(let v): UserDefaults.standard.set(v, forKey: key)
            }
            return
        }
        let snapshot = values.withLock { dict -> [String: PreferenceValue] in
            dict[key] = value
            return dict
        }
        try? FileManager.default.createDirectory(at: fileURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? JSONEncoder().encode(snapshot).write(to: fileURL, options: .atomic)
    }
}

/// User-facing app settings (observable, persisted through `Preferences`).
@MainActor
@Observable
final class AppSettings {
    static let shared = AppSettings()

    /// "Crea alias sulla Scrivania": a Finder alias on ~/Desktop for every mounted volume.
    var createDesktopAlias: Bool {
        didSet {
            guard createDesktopAlias != oldValue else { return }
            Preferences.shared.set(createDesktopAlias, forKey: "createDesktopAlias")
            onChange?()
        }
    }

    @ObservationIgnored var onChange: (@MainActor () -> Void)?

    private init() {
        createDesktopAlias = Preferences.shared.bool(forKey: "createDesktopAlias")
    }
}

enum AppPaths {
    /// True when `LUMENDISK_SUPPORT_DIR` redirects all app state (automated launches).
    static var isIsolated: Bool {
        !(ProcessInfo.processInfo.environment["LUMENDISK_SUPPORT_DIR"] ?? "").isEmpty
    }

    static var supportDirectory: URL {
        if let custom = ProcessInfo.processInfo.environment["LUMENDISK_SUPPORT_DIR"], !custom.isEmpty {
            return URL(fileURLWithPath: (custom as NSString).expandingTildeInPath, isDirectory: true)
        }
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Library/Application Support")
        return base.appendingPathComponent("LumenDisk", isDirectory: true)
    }

    /// Parent folder for new mountpoints; nil lets the core choose (/Volumes if writable, else
    /// ~/Volumes). Debug builds honour `LUMENDISK_MOUNT_ROOT` so automated runs stay in a
    /// scratch folder.
    static var defaultMountRoot: String? {
        #if DEBUG
        if let custom = ProcessInfo.processInfo.environment["LUMENDISK_MOUNT_ROOT"], !custom.isEmpty { return custom }
        #endif
        return nil
    }

    /// `.icns` handed to the core as `volume_icon`, shown by Finder for every LumenDisk volume.
    /// Order: the app bundle (Contents/Resources/VolumeIcon.icns), then Resources/ next to the
    /// Swift package (development runs), then the app icon as a last resort.
    static let volumeIconPath: String? = {
        let fm = FileManager.default
        var candidates: [URL] = []
        if Bundle.main.bundleURL.pathExtension == "app" {
            if let url = Bundle.main.url(forResource: "VolumeIcon", withExtension: "icns") { candidates.append(url) }
        }
        // <package>/Sources/LumenDisk/Services/Preferences.swift → <package>/Resources
        let packageResources = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Resources", isDirectory: true)
        candidates.append(packageResources.appendingPathComponent("VolumeIcon.icns"))
        if let appIcon = Bundle.main.url(forResource: "AppIcon", withExtension: "icns") { candidates.append(appIcon) }
        return candidates.first { fm.fileExists(atPath: $0.path) }?.path
    }()
}
