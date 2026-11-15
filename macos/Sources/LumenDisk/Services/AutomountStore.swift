import Foundation
import Observation

/// Per-volume automount preference, keyed by the filesystem UUID reported by the core, with
/// extra aliases (GPT partition UUID, DiskArbitration volume UUID, image path) so that a disk can
/// be recognised by DiskMonitor before the core has ever opened it.
struct AutomountRule: Codable, Sendable, Hashable, Identifiable {
    var id: UUID = UUID()
    var volumeUUID: String
    var aliases: [String] = []
    var displayName: String
    var fsType: String
    var mode: MountMode = .readWrite
    var ghostShield: Bool = true
    var uidMapEnabled: Bool = true
    var customIDs: Bool = false
    var linuxUid: UInt32 = 1000
    var linuxGid: UInt32 = 1000
    var automount: Bool = true
    var lastSourcePath: String?
    var createdAt: Date = Date()

    var filesystem: FilesystemKind { FilesystemKind(coreName: fsType) }

    var identifiers: Set<String> {
        Set(([volumeUUID] + aliases).filter { !$0.isEmpty }.map { $0.uppercased() })
    }

    var uidMapRequest: UIDMapRequest {
        UIDMapRequest(enabled: uidMapEnabled,
                      linuxUid: customIDs ? linuxUid : nil,
                      linuxGid: customIDs ? linuxGid : nil)
    }

    func matches(_ candidates: [String?]) -> Bool {
        let ids = identifiers
        return candidates.contains { candidate in
            guard let candidate, !candidate.isEmpty else { return false }
            return ids.contains(candidate.uppercased())
        }
    }
}

/// Persists automount rules as JSON in ~/Library/Application Support/LumenDisk.
@MainActor
@Observable
final class AutomountStore {
    private(set) var rules: [AutomountRule] = []
    @ObservationIgnored private let fileURL: URL
    @ObservationIgnored var onChange: (() -> Void)?

    init(directory: URL = AppPaths.supportDirectory) {
        fileURL = directory.appendingPathComponent("automount-rules.json")
        load()
    }

    func rule(matching candidates: [String?]) -> AutomountRule? {
        rules.first { $0.matches(candidates) }
    }

    func rule(forVolumeUUID uuid: String) -> AutomountRule? {
        rules.first { $0.volumeUUID.caseInsensitiveCompare(uuid) == .orderedSame }
    }

    /// Identifiers (uppercased) of rules that want LumenDisk to take over the disk on plug-in.
    var automountIdentifiers: Set<String> {
        rules.filter(\.automount).reduce(into: Set<String>()) { $0.formUnion($1.identifiers) }
    }

    func upsert(_ rule: AutomountRule) {
        if let index = rules.firstIndex(where: { $0.id == rule.id }) {
            rules[index] = rule
        } else {
            rules.append(rule)
        }
        save()
    }

    func update(_ id: UUID, _ change: (inout AutomountRule) -> Void) {
        guard let index = rules.firstIndex(where: { $0.id == id }) else { return }
        change(&rules[index])
        save()
    }

    func delete(_ id: UUID) {
        rules.removeAll { $0.id == id }
        save()
    }

    /// Adds aliases discovered later (e.g. the DA media UUID once the volume is mounted).
    func addAliases(_ aliases: [String?], toRuleWithVolumeUUID uuid: String, sourcePath: String?) {
        guard let index = rules.firstIndex(where: { $0.volumeUUID.caseInsensitiveCompare(uuid) == .orderedSame }) else { return }
        var rule = rules[index]
        var set = Set(rule.aliases.map { $0.uppercased() })
        var changed = false
        for alias in aliases.compactMap({ $0 }) where !alias.isEmpty && !set.contains(alias.uppercased()) && alias.caseInsensitiveCompare(uuid) != .orderedSame {
            rule.aliases.append(alias)
            set.insert(alias.uppercased())
            changed = true
        }
        if let sourcePath, rule.lastSourcePath != sourcePath { rule.lastSourcePath = sourcePath; changed = true }
        guard changed else { return }
        rules[index] = rule
        save()
    }

    private func load() {
        guard let data = try? Data(contentsOf: fileURL) else { return }
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        rules = (try? decoder.decode([AutomountRule].self, from: data)) ?? []
    }

    private func save() {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        encoder.dateEncodingStrategy = .iso8601
        do {
            try FileManager.default.createDirectory(at: fileURL.deletingLastPathComponent(), withIntermediateDirectories: true)
            try encoder.encode(rules).write(to: fileURL, options: .atomic)
        } catch {
            NSLog("LumenDisk: could not save automount rules: \(error)")
        }
        onChange?()
    }
}
