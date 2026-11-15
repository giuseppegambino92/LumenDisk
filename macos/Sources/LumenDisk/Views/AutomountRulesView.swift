import SwiftUI

struct AutomountRulesView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            if model.rules.rules.isEmpty {
                ContentUnavailableView {
                    Label(L("Nessuna regola automatica", "No automount rules"), systemImage: "wand.and.sparkles")
                } description: {
                    Text(L("Le regole ricordano come montare ogni disco (modalità, Ghost Shield, permessi) e vengono applicate appena lo colleghi. Creane una dalla scheda di un volume montato.",
                           "Rules remember how to mount each disk (mode, Ghost Shield, permissions) and are applied as soon as you connect it. Create one from a mounted volume."))
                } actions: {
                    addMenu
                        .buttonStyle(.borderedProminent)
                        .fixedSize()
                }
            } else {
                DetailCanvas {
                    PageHeader(title: L("Regole Automatiche", "Automount Rules"),
                               subtitle: L("LumenDisk riconosce i dischi tramite UUID e applica queste preferenze appena vengono collegati o rimontati.",
                                           "LumenDisk recognises disks by their UUID and applies these preferences as soon as they are connected or remounted."),
                               systemImage: "wand.and.sparkles", tint: .purple)
                    ForEach(model.rules.rules) { rule in
                        RuleCard(rule: rule)
                    }
                }
            }
        }
        .navigationTitle(L("Regole Automatiche", "Automount Rules"))
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                addMenu
                    .help(L("Aggiungi regola da un volume montato", "Add a rule from a mounted volume"))
            }
        }
    }

    private var addMenu: some View {
        Menu {
            let candidates = model.localVolumes.filter { model.rule(for: $0) == nil }
            if candidates.isEmpty {
                Text(L("Nessun volume montato senza regola", "No mounted volume without a rule"))
            }
            ForEach(candidates) { volume in
                Button {
                    _ = model.makeRule(from: volume)
                } label: {
                    Label(volume.displayName, systemImage: volume.symbol)
                }
            }
        } label: {
            Label(L("Aggiungi da Volume Montato", "Add from Mounted Volume"), systemImage: "plus")
        }
    }
}

private struct RuleCard: View {
    @Environment(AppModel.self) private var model
    var rule: AutomountRule
    @State private var confirmDelete = false

    private func binding<T>(_ keyPath: WritableKeyPath<AutomountRule, T>) -> Binding<T> {
        Binding(get: { rule[keyPath: keyPath] },
                set: { value in model.rules.update(rule.id) { $0[keyPath: keyPath] = value } })
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 12) {
                IconBadge(systemImage: rule.lastSourcePath.flatMap(DeviceNode.bsdName(fromPath:)) != nil ? "externaldrive.fill" : "cylinder.split.1x2.fill",
                          tint: rule.filesystem.tint, size: 38)
                VStack(alignment: .leading, spacing: 4) {
                    HStack(spacing: 6) {
                        Text(rule.displayName).font(.headline)
                        FilesystemPill(kind: rule.filesystem, compact: true)
                    }
                    Text(rule.lastSourcePath.map { URL(fileURLWithPath: $0).lastPathComponent } ?? rule.filesystem.familyName)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .help("UUID \(rule.volumeUUID)")
                }
                Spacer()
                Toggle(L("Monta automaticamente", "Mount automatically"), isOn: binding(\.automount))
                    .toggleStyle(.switch)
                Button {
                    confirmDelete = true
                } label: {
                    Image(systemName: "trash")
                }
                .buttonStyle(.borderless)
                .accessibilityLabel(L("Elimina regola", "Delete rule"))
                .help(L("Elimina regola", "Delete rule"))
            }

            Picker(L("Modalità", "Mode"), selection: binding(\.mode)) {
                ForEach(MountMode.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()

            HStack(alignment: .top, spacing: 28) {
                Toggle(isOn: binding(\.ghostShield)) {
                    VStack(alignment: .leading, spacing: 1) {
                        Text("Ghost Shield")
                        Text(L("Blocca .DS_Store e ._*", "Blocks .DS_Store and ._*")).font(.caption).foregroundStyle(.secondary)
                    }
                }
                Toggle(isOn: binding(\.uidMapEnabled)) {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(L("Mappatura Permessi Linux", "Linux Permission Mapping"))
                        Text(L("UID 501 ↔ 1000 automatico", "UID 501 ↔ 1000 automatic")).font(.caption).foregroundStyle(.secondary)
                    }
                }
                Toggle(isOn: binding(\.customIDs)) {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(L("UID/GID personalizzati", "Custom UID/GID"))
                        Text(L("Invece del rilevamento automatico", "Instead of automatic detection")).font(.caption).foregroundStyle(.secondary)
                    }
                }
                .disabled(!rule.uidMapEnabled)
            }
            .toggleStyle(.checkbox)

            if rule.uidMapEnabled && rule.customIDs {
                HStack(spacing: 10) {
                    Text(L("UID Linux", "Linux UID")).foregroundStyle(.secondary)
                    TextField("UID", value: binding(\.linuxUid), format: .number.grouping(.never))
                        .font(.system(.body, design: .rounded))
                        .frame(width: 80)
                    Text(L("GID Linux", "Linux GID")).foregroundStyle(.secondary)
                    TextField("GID", value: binding(\.linuxGid), format: .number.grouping(.never))
                        .font(.system(.body, design: .rounded))
                        .frame(width: 80)
                    TagBadge(text: "501:20 ↔ \(rule.linuxUid):\(rule.linuxGid)")
                }
                .textFieldStyle(.roundedBorder)
                .font(.callout)
            }

            if let source = rule.lastSourcePath {
                Text(L("Ultima origine: \(source)", "Last source: \(source)"))
                    .font(.caption)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        }
        .card()
        .confirmationDialog(L("Eliminare la regola per «\(rule.displayName)»?", "Delete the rule for “\(rule.displayName)”?"), isPresented: $confirmDelete, titleVisibility: .visible) {
            Button(L("Elimina Regola", "Delete Rule"), role: .destructive) { model.rules.delete(rule.id) }
            Button(L("Annulla", "Cancel"), role: .cancel) {}
        }
    }
}
