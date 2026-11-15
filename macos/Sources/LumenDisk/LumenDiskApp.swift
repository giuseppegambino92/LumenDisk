import AppKit
import SwiftUI

@main
struct LumenDiskApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @State private var model = AppModel.shared
    @State private var languageStore = LanguageStore.shared

    var body: some Scene {
        // Reading the language here re-evaluates the scenes (menus included) when it changes.
        let language = languageStore.language

        Window("LumenDisk", id: WindowID.main) {
            MainWindowView()
                .environment(model)
                .environment(\.locale, language.locale)
                .frame(minWidth: 900, minHeight: 600)
        }
        .defaultSize(width: 1040, height: 700)
        .windowResizability(.contentMinSize)
        .windowToolbarStyle(.unifiedCompact)
        .commands { LumenDiskCommands(model: model, language: language) }

        MenuBarExtra("LumenDisk", systemImage: "externaldrive.connected.to.line.below") {
            MenuBarPopoverView()
                .environment(model)
                .environment(\.locale, language.locale)
        }
        .menuBarExtraStyle(.window)

        Settings {
            SettingsView()
                .environment(model)
                .environment(\.locale, language.locale)
        }
    }
}

enum WindowID {
    static let main = "main"
}

struct LumenDiskCommands: Commands {
    let model: AppModel
    let language: AppLanguage
    @Environment(\.openWindow) private var openWindow

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button(L("Monta Immagine Disco…", "Mount Disk Image…")) {
                openWindow(id: WindowID.main)
                ImagePicker.presentOpenPanel(model: model)
            }
            .keyboardShortcut("o")

            Button(L("Monta Immagine da URL…", "Mount Image from URL…")) {
                openWindow(id: WindowID.main)
                model.activeSheet = .imageURL
            }
            .keyboardShortcut("o", modifiers: [.command, .shift])

            Button(L("Connetti a Server…", "Connect to Server…")) {
                openWindow(id: WindowID.main)
                model.activeSheet = .connectServer
            }
            .keyboardShortcut("k")

            Button(L("Crea Disco Virtuale Test…", "Create Test Virtual Disk…")) {
                openWindow(id: WindowID.main)
                model.selection = .images
                model.showCreateWizard = true
            }
            .keyboardShortcut("n")
        }
        CommandGroup(after: .newItem) {
            Divider()
            Button(L("Espelli Tutti in Sicurezza", "Eject All Safely")) {
                Task { await model.ejectAll() }
            }
            .keyboardShortcut("e", modifiers: [.command, .shift])
            .disabled(model.volumes.isEmpty)

            Button(L("Aggiorna", "Refresh")) {
                Task { await model.refresh(force: true) }
            }
            .keyboardShortcut("r")
        }
        CommandMenu(L("Lingua", "Language")) {
            ForEach(AppLanguage.allCases) { option in
                Toggle(option.nativeName, isOn: Binding(
                    get: { language == option },
                    set: { if $0 { LanguageStore.shared.language = option } }
                ))
            }
        }
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationWillFinishLaunching(_ notification: Notification) {
        #if DEBUG
        DebugHooks.applyEarly()
        #endif
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        Task { await AppModel.shared.start() }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    func applicationWillTerminate(_ notification: Notification) {
        AppModel.shared.prepareForQuit()
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        let model = AppModel.shared
        if model.isTerminating || model.volumes.isEmpty { return .terminateNow }

        let count = model.volumes.count
        let pending = model.pendingShadowVolumes
        let unsynced = model.pendingSyncVolumes
        let alert = NSAlert()
        alert.alertStyle = .informational
        alert.messageText = count == 1
            ? L("Espellere il volume prima di uscire?", "Eject the volume before quitting?")
            : L("Espellere i \(count) volumi prima di uscire?", "Eject the \(count) volumes before quitting?")
        var info = L("I volumi di LumenDisk sono serviti dall'app stessa: uscendo verranno espulsi in sicurezza, così nessun dato resta in sospeso.",
                     "LumenDisk volumes are served by the app itself: quitting ejects them safely, so no data is left hanging.")
        if !pending.isEmpty {
            let changes = pending.reduce(UInt64(0)) { $0 + ($1.shadow?.totalChanges ?? 0) }
            let names = pending.map(\.displayName).joined(separator: ", ")
            info += "\n\n" + L("Ci sono \(changes) modifiche in Shadow Mode non ancora applicate su \(names). Se non le applichi andranno perse.",
                               "There are \(changes) Shadow Mode changes not yet applied on \(names). If you don’t apply them they will be lost.")
        }
        if !unsynced.isEmpty {
            let files = unsynced.reduce(UInt64(0)) { $0 + ($1.remote?.pendingOps ?? 0) }
            let names = unsynced.map(\.displayName).joined(separator: ", ")
            info += "\n\n" + L10n.plural(files,
                                         one: ("%d file su \(names) è in attesa: verrà sincronizzato al prossimo collegamento.",
                                               "%d file on \(names) is waiting: it will sync next time you connect."),
                                         other: ("%d file su \(names) sono in attesa: verranno sincronizzati al prossimo collegamento.",
                                                 "%d files on \(names) are waiting: they will sync next time you connect."))
        }
        alert.informativeText = info
        alert.addButton(withTitle: L("Espelli ed Esci", "Eject and Quit"))
        alert.addButton(withTitle: L("Annulla", "Cancel"))
        if !pending.isEmpty { alert.addButton(withTitle: L("Rivedi Modifiche…", "Review Changes…")) }

        NSApp.activate()
        switch alert.runModal() {
        case .alertFirstButtonReturn:
            break
        case .alertThirdButtonReturn:
            model.shadowFocusID = pending.first?.id
            model.selection = .shadow
            model.bringMainWindowForward()
            return .terminateCancel
        default:
            return .terminateCancel
        }

        model.isTerminating = true
        Task { @MainActor in
            let failed = await model.ejectAllForQuit()
            var proceed = true
            if !failed.isEmpty {
                let warning = NSAlert()
                warning.alertStyle = .warning
                warning.messageText = L("Alcuni volumi sono ancora in uso", "Some volumes are still in use")
                warning.informativeText = L("Non è stato possibile espellere \(failed.joined(separator: ", ")). Se esci ora, le app che li stanno usando perderanno l'accesso ai file.",
                                            "Could not eject \(failed.joined(separator: ", ")). If you quit now, the apps using them will lose access to their files.")
                warning.addButton(withTitle: L("Annulla", "Cancel"))
                warning.addButton(withTitle: L("Esci Comunque", "Quit Anyway"))
                proceed = warning.runModal() == .alertSecondButtonReturn
                if !proceed {
                    model.isTerminating = false
                    await model.refresh(force: true)
                }
            }
            NSApp.reply(toApplicationShouldTerminate: proceed)
        }
        return .terminateLater
    }
}
