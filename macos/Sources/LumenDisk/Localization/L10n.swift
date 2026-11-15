import CLumenDisk
import Foundation
import Observation
import os

/// Languages the interface is translated into.
enum AppLanguage: String, CaseIterable, Identifiable, Sendable {
    case italian = "it"
    case english = "en"

    var id: String { rawValue }

    /// Always shown in its own language, as macOS does in language pickers.
    var nativeName: String {
        switch self {
        case .italian: "Italiano"
        case .english: "English"
        }
    }

    var flag: String {
        switch self {
        case .italian: "🇮🇹"
        case .english: "🇬🇧"
        }
    }

    /// Locale used for numbers, byte counts and relative dates. Keeps the user's own region when
    /// it already speaks the chosen language (e.g. en_GB stays en_GB).
    var locale: Locale {
        if Locale.current.language.languageCode?.identifier == rawValue { return Locale.current }
        return Locale(identifier: self == .italian ? "it_IT" : "en_US")
    }

    /// The first preferred system language if it is Italian or English, otherwise Italian.
    static var systemDefault: AppLanguage {
        guard let first = Locale.preferredLanguages.first else { return .italian }
        switch Locale(identifier: first).language.languageCode?.identifier {
        case "en"?: return .english
        default: return .italian
        }
    }
}

/// Runtime language switch, persisted in the app preferences. Views pick up a change
/// automatically: every translated string reads `language` through `L10n`, which registers an
/// Observation dependency while a SwiftUI body is being evaluated.
@MainActor
@Observable
final class LanguageStore {
    static let shared = LanguageStore()

    private static let key = "language"

    var language: AppLanguage {
        didSet {
            guard language != oldValue else { return }
            L10n.mirror(language)
            if persists { Preferences.shared.set(language.rawValue, forKey: Self.key) }
        }
    }

    @ObservationIgnored private var persists = true

    private init() {
        let stored = Preferences.shared.string(forKey: Self.key).flatMap(AppLanguage.init(rawValue:))
        language = stored ?? AppLanguage.systemDefault
        L10n.mirror(language)
    }

    /// Debug/test override that is not written to the preferences.
    func override(_ language: AppLanguage) {
        persists = false
        self.language = language
        persists = true
    }
}

/// Central translation entry point. Thread-safe: off the main thread it reads a lock-protected
/// mirror of the current language (used by error descriptions built inside the core actor).
enum L10n {
    private static let current = OSAllocatedUnfairLock(initialState: AppLanguage.systemDefault)

    fileprivate static func mirror(_ language: AppLanguage) {
        current.withLock { $0 = language }
        // Core error messages and status reasons follow the app language too.
        // (Synchronous, thread-safe and instantaneous: it only flips a flag in the core.)
        if let reply = ld_set_language(language.rawValue) {
            ld_free_string(reply)
        }
    }

    static var language: AppLanguage {
        if Thread.isMainThread {
            // Touch the observable store so that SwiftUI bodies re-render on change.
            return MainActor.assumeIsolated { LanguageStore.shared.language }
        }
        return current.withLock { $0 }
    }

    static var isEnglish: Bool { language == .english }
    static var locale: Locale { language.locale }

    static func tr(_ it: String, _ en: String) -> String {
        language == .english ? en : it
    }

    /// Simple count-based plural choice ("1 file" / "3 files").
    static func plural(_ count: some BinaryInteger, one: (String, String), other: (String, String)) -> String {
        let pair = count == 1 ? one : other
        return tr(pair.0, pair.1).replacingOccurrences(of: "%d", with: "\(count)")
    }
}

/// Shorthand used by every view: `L("Espelli in Sicurezza", "Eject Safely")`.
func L(_ it: String, _ en: String) -> String { L10n.tr(it, en) }
