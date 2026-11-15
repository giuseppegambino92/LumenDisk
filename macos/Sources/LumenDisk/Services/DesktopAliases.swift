import AppKit
import Foundation

/// "Crea alias sulla Scrivania": keeps one real Finder alias on ~/Desktop per mounted volume.
///
/// Safety rules: an alias is only ever removed if (1) LumenDisk recorded creating it, (2) the file
/// is still a Finder alias, and (3) the bookmark stored inside it points into the volume's mount
/// point. Anything else on the Desktop is never touched. The ledger is persisted so that aliases
/// left behind by a crash are cleaned up on the next launch.
@MainActor
final class DesktopAliasManager {
    private struct Entry: Codable, Hashable {
        var aliasPath: String
        var mountpoint: String
    }

    private var ledger: [String: Entry] = [:]   // volume id → alias
    private let ledgerURL: URL
    private let desktop: URL

    init(supportDirectory: URL = AppPaths.supportDirectory) {
        ledgerURL = supportDirectory.appendingPathComponent("desktop-aliases.json")
        desktop = FileManager.default.urls(for: .desktopDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Desktop", isDirectory: true)
        if let data = try? Data(contentsOf: ledgerURL),
           let stored = try? JSONDecoder().decode([String: Entry].self, from: data) {
            ledger = stored
        }
    }

    /// Creates aliases for new volumes and removes the ones whose volume is gone.
    func sync(volumes: [VolumeStatus], enabled: Bool) {
        let mounted = Dictionary(volumes.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        var changed = false
        for (id, entry) in ledger where !enabled || mounted[id]?.mountpoint != entry.mountpoint {
            removeAlias(entry)
            ledger[id] = nil
            changed = true
        }
        if enabled {
            for volume in volumes where ledger[volume.id] == nil && !volume.mountpoint.isEmpty {
                if let entry = createAlias(for: volume) {
                    ledger[volume.id] = entry
                    changed = true
                }
            }
        }
        if changed { save() }
    }

    /// Removes every alias LumenDisk created (quit, or before ejecting everything).
    func removeAll() {
        guard !ledger.isEmpty else { return }
        for entry in ledger.values { removeAlias(entry) }
        ledger.removeAll()
        save()
    }

    func remove(volumeID: String) {
        guard let entry = ledger.removeValue(forKey: volumeID) else { return }
        removeAlias(entry)
        save()
    }

    // MARK: Private

    private func createAlias(for volume: VolumeStatus) -> Entry? {
        let target = URL(fileURLWithPath: volume.mountpoint, isDirectory: true)
        guard FileManager.default.fileExists(atPath: target.path) else { return nil }
        let base = sanitized(volume.displayName)
        var candidate = desktop.appendingPathComponent(base)
        var attempt = 1
        while FileManager.default.fileExists(atPath: candidate.path) || isSymlink(candidate) {
            // An alias we made earlier for this same mount point can be reused.
            if pointsInto(candidate, mountpoint: volume.mountpoint) {
                return Entry(aliasPath: candidate.path, mountpoint: volume.mountpoint)
            }
            attempt += 1
            guard attempt < 50 else { return nil }
            candidate = desktop.appendingPathComponent("\(base) \(attempt)")
        }
        do {
            let data = try target.bookmarkData(options: .suitableForBookmarkFile, includingResourceValuesForKeys: nil, relativeTo: nil)
            try URL.writeBookmarkData(data, to: candidate)
            // NFS volume roots can't carry a custom-icon flag, so the alias wears the LumenDisk
            // volume icon instead (stored in the alias file's own resource fork).
            if let iconPath = AppPaths.volumeIconPath, let image = NSImage(contentsOfFile: iconPath) {
                NSWorkspace.shared.setIcon(image, forFile: candidate.path, options: [])
            }
            return Entry(aliasPath: candidate.path, mountpoint: volume.mountpoint)
        } catch {
            NSLog("LumenDisk: could not create Desktop alias for \(volume.mountpoint): \(error.localizedDescription)")
            return nil
        }
    }

    private func removeAlias(_ entry: Entry) {
        let url = URL(fileURLWithPath: entry.aliasPath)
        guard pointsInto(url, mountpoint: entry.mountpoint) else { return }
        try? FileManager.default.removeItem(at: url)
    }

    /// True only for a Finder alias file whose stored target lies inside `mountpoint`.
    /// The bookmark is read without resolving it, so this also works after the unmount.
    private func pointsInto(_ url: URL, mountpoint: String) -> Bool {
        guard let values = try? url.resourceValues(forKeys: [.isAliasFileKey, .isSymbolicLinkKey]),
              values.isAliasFile == true, values.isSymbolicLink != true,
              let data = try? URL.bookmarkData(withContentsOf: url),
              let stored = URL.resourceValues(forKeys: [.pathKey], fromBookmarkData: data),
              let path = stored.path else { return false }
        let root = mountpoint.hasSuffix("/") ? String(mountpoint.dropLast()) : mountpoint
        return path == root || path.hasPrefix(root + "/")
    }

    private func isSymlink(_ url: URL) -> Bool {
        (try? url.resourceValues(forKeys: [.isSymbolicLinkKey]).isSymbolicLink) ?? false
    }

    private func sanitized(_ name: String) -> String {
        let cleaned = name.replacingOccurrences(of: "/", with: "-").replacingOccurrences(of: ":", with: "-")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return cleaned.isEmpty || cleaned.hasPrefix(".") ? "LumenDisk " + cleaned : cleaned
    }

    private func save() {
        do {
            try FileManager.default.createDirectory(at: ledgerURL.deletingLastPathComponent(), withIntermediateDirectories: true)
            if ledger.isEmpty {
                try? FileManager.default.removeItem(at: ledgerURL)
            } else {
                try JSONEncoder().encode(ledger).write(to: ledgerURL, options: .atomic)
            }
        } catch {
            NSLog("LumenDisk: could not save the Desktop alias ledger: \(error.localizedDescription)")
        }
    }
}
