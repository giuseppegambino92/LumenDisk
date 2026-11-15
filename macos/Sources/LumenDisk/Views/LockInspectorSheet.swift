import AppKit
import SwiftUI

/// "Chi Blocca il Disco?" / "Who's Using the Disk?" — lists the processes holding files open on a LumenDisk volume.
struct LockInspectorSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    var inspection: LockInspection

    @State private var holders: [LockHolder] = []
    @State private var selected: Set<Int32> = []
    @State private var isLoading = true
    @State private var isReleasing = false
    @State private var loadError: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
                .padding(24)
            Divider()
            content
                .frame(minHeight: 180, maxHeight: 340)
            Divider()
            footer
                .padding(.horizontal, 24)
                .padding(.vertical, 16)
        }
        .frame(width: 560)
        .task { await load() }
    }

    private var header: some View {
        HStack(alignment: .top, spacing: 14) {
            IconBadge(systemImage: "lock.rectangle.stack.fill", tint: .orange, size: 44)
            VStack(alignment: .leading, spacing: 4) {
                Text(L("Chi Blocca il Disco?", "Who’s Using the Disk?")).font(.title3.weight(.semibold))
                Text(inspection.failureMessage == nil
                     ? L("Processi che stanno usando file su «\(inspection.volumeName)».", "Processes using files on “\(inspection.volumeName)”.")
                     : L("«\(inspection.volumeName)» non può essere espulso perché alcune app stanno ancora usando dei file.",
                         "“\(inspection.volumeName)” can’t be ejected because some apps are still using files on it."))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if let failure = inspection.failureMessage {
                    Text(failure)
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                        .lineLimit(2)
                }
            }
            Spacer()
            Button {
                Task { await load() }
            } label: {
                Image(systemName: "arrow.clockwise")
            }
            .buttonStyle(.borderless)
            .disabled(isLoading)
            .accessibilityLabel(L("Ispeziona Blocchi di nuovo", "Check again"))
            .help(L("Ispeziona Blocchi", "Check Again"))
        }
    }

    @ViewBuilder
    private var content: some View {
        if isLoading && holders.isEmpty {
            VStack(spacing: 10) {
                ProgressView()
                Text(L("Ispezione dei blocchi…", "Looking for open files…")).foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if let loadError {
            ContentUnavailableView(L("Impossibile ispezionare i blocchi", "Couldn’t check for open files"), systemImage: "exclamationmark.triangle",
                                   description: Text(loadError))
        } else if holders.isEmpty {
            ContentUnavailableView {
                Label(L("Nessun processo blocca il disco", "Nothing is using the disk"), systemImage: "checkmark.shield")
            } description: {
                Text(L("Puoi espellere «\(inspection.volumeName)» in sicurezza.", "You can eject “\(inspection.volumeName)” safely."))
            }
        } else {
            ScrollView {
                VStack(spacing: 0) {
                    ForEach(Array(holders.enumerated()), id: \.element.id) { index, holder in
                        if index > 0 { Divider().padding(.leading, 84) }
                        HolderRow(holder: holder, mountpoint: inspection.mountpoint, isSelected: Binding(
                            get: { selected.contains(holder.pid) },
                            set: { if $0 { selected.insert(holder.pid) } else { selected.remove(holder.pid) } }
                        ))
                    }
                }
                .padding(.vertical, 6)
            }
        }
    }

    private var footer: some View {
        HStack(spacing: 10) {
            if !holders.isEmpty {
                Label(L("Le app selezionate verranno chiuse: salva prima il tuo lavoro.", "The selected apps will be closed: save your work first."), systemImage: "info.circle")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Button(L("Annulla", "Cancel"), role: .cancel) { dismiss() }
                .keyboardShortcut(.cancelAction)
            Button {
                Task {
                    isReleasing = true
                    let ok = await model.releaseLocksAndEject(inspection, pids: holders.isEmpty ? [] : Array(selected))
                    isReleasing = false
                    if ok { dismiss() } else { await load() }
                }
            } label: {
                if isReleasing {
                    HStack(spacing: 6) { ProgressView().controlSize(.small); Text(L("Espulsione…", "Ejecting…")) }
                } else {
                    Text(holders.isEmpty ? L("Espelli Ora", "Eject Now") : L("Sblocca ed Espelli", "Release and Eject"))
                }
            }
            .buttonStyle(.borderedProminent)
            .keyboardShortcut(.defaultAction)
            .disabled(isReleasing || isLoading || (!holders.isEmpty && selected.isEmpty))
        }
        .controlSize(.large)
    }

    private func load() async {
        isLoading = true
        defer { isLoading = false }
        do {
            let latest = try await model.lockHolders(mountpoint: inspection.mountpoint)
            holders = latest.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
            selected = Set(latest.map(\.pid))
            loadError = nil
        } catch {
            loadError = error.localizedDescription
        }
    }
}

private struct HolderRow: View {
    var holder: LockHolder
    var mountpoint: String
    @Binding var isSelected: Bool

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Toggle(L("Chiudi \(holder.name)", "Quit \(holder.name)"), isOn: $isSelected)
                .toggleStyle(.checkbox)
                .labelsHidden()
                .padding(.top, 8)
            Image(nsImage: ProcessIcon.icon(pid: holder.pid, executable: holder.executable))
                .resizable()
                .interpolation(.high)
                .frame(width: 36, height: 36)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 8) {
                    Text(holder.name).font(.body.weight(.medium))
                    TagBadge(text: "PID \(holder.pid)")
                }
                ForEach(holder.files.prefix(3), id: \.self) { file in
                    Text(relative(file))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .help(file)
                }
                if holder.files.count > 3 {
                    Text(L("e altri \(holder.files.count - 3) file", "and \(holder.files.count - 3) more files"))
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 24)
        .padding(.vertical, 10)
        .contentShape(Rectangle())
        .onTapGesture { isSelected.toggle() }
        .accessibilityElement(children: .combine)
    }

    private func relative(_ file: String) -> String {
        guard file.hasPrefix(mountpoint) else { return file }
        let rest = file.dropFirst(mountpoint.count)
        return rest.isEmpty ? L("/ (cartella principale)", "/ (top folder)") : String(rest)
    }
}
