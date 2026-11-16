import Foundation

// Non-view logic of the Forge screens: filesystem rules (labels, cluster sizes), validation of
// what the user typed, request building from the UI state, and number formatting. Everything
// here is a pure function of its inputs, so it is covered by the unit tests.

// MARK: - Filesystems

extension ForgeFS {
    var displayName: String {
        switch self {
        case .ext2: "ext2"
        case .ext3: "ext3"
        case .ext4: "ext4"
        case .fat16: "FAT16"
        case .fat32: "FAT32"
        case .exfat: "exFAT"
        case .ntfs: "NTFS"
        case .btrfs: "Btrfs"
        case .xfs: "XFS"
        case .f2fs: "F2FS"
        case .ufs2: "UFS2"
        }
    }

    /// Where the filesystem is at home, for the picker.
    var useHint: String {
        switch self {
        case .exfat: L("Mac, Windows e Linux; file oltre 4 GB", "Mac, Windows and Linux; files over 4 GB")
        case .fat32: L("Massima compatibilità (file fino a 4 GB)", "Widest compatibility (files up to 4 GB)")
        case .fat16: L("Dispositivi datati, fino a 4 GB", "Legacy devices, up to 4 GB")
        case .ntfs: "Windows"
        case .ext4, .ext3, .ext2: "Linux"
        case .btrfs, .xfs: L("Linux (server, NAS)", "Linux (servers, NAS)")
        case .f2fs: L("Linux su memorie flash", "Linux on flash storage")
        case .ufs2: "FreeBSD"
        }
    }

    enum LabelUnit: Sendable { case utf8Bytes, utf16Units }

    /// Longest label the filesystem stores, and in which unit it is counted (format.rs
    /// `sanitize_label`).
    var labelLimit: (max: Int, unit: LabelUnit) {
        switch self {
        case .fat16, .fat32: (11, .utf8Bytes)
        case .exfat: (11, .utf16Units)
        case .ntfs: (32, .utf16Units)
        case .ext2, .ext3, .ext4: (16, .utf8Bytes)
        case .xfs: (12, .utf8Bytes)
        case .btrfs: (255, .utf8Bytes)
        case .f2fs: (512, .utf16Units)
        case .ufs2: (31, .utf8Bytes)
        }
    }

    /// The label exactly as the core will write it (mirror of format.rs `sanitize_label`):
    /// control characters dropped; FAT upper-cased and limited to ASCII without the characters
    /// FAT forbids; UFS2 only letters, digits, `_` and `-`; then cut to the limit.
    func sanitizedLabel(_ label: String) -> String {
        let forbiddenFAT = Set("\"*+,./:;<=>?[\\]|".unicodeScalars)
        var scalars: [Unicode.Scalar] = label.unicodeScalars.filter { $0.properties.generalCategory != .control }
        if self == .ufs2 {
            scalars = scalars.map { s in
                (s.isASCII && (s.properties.isAlphabetic || ("0"..."9").contains(s))) || s == "_" || s == "-" ? s : "_"
            }
        }
        if self == .fat16 || self == .fat32 {
            var view = String.UnicodeScalarView()
            view.append(contentsOf: scalars)
            let upper = String(view).uppercased()
            scalars = Array(upper.unicodeScalars.filter { $0.isASCII && !forbiddenFAT.contains($0) })
        }
        let (max, unit) = labelLimit
        var out = String.UnicodeScalarView()
        var used = 0
        for s in scalars {
            let cost = unit == .utf8Bytes ? String(s).utf8.count : String(s).utf16.count
            if used + cost > max { break }
            used += cost
            out.append(s)
        }
        var result = String(out)
        if self == .fat16 || self == .fat32 {
            while result.hasSuffix(" ") { result.removeLast() }
        }
        return result
    }

    /// How much of the limit `label` uses, in the filesystem's unit (before sanitising).
    func labelLength(_ label: String) -> Int {
        labelLimit.unit == .utf8Bytes ? label.utf8.count : label.utf16.count
    }

    var labelRule: String {
        let (max, _) = labelLimit
        switch self {
        case .fat16, .fat32:
            return L("Fino a \(max) caratteri, maiuscoli, senza \" * + , . / : ; < = > ? [ \\ ] |",
                     "Up to \(max) characters, upper case, without \" * + , . / : ; < = > ? [ \\ ] |")
        case .ufs2:
            return L("Fino a \(max) caratteri: lettere, cifre, _ e -", "Up to \(max) characters: letters, digits, _ and -")
        case .ext2, .ext3, .ext4, .xfs, .btrfs:
            return L("Fino a \(max) byte (le lettere accentate ne occupano 2)", "Up to \(max) bytes (accented letters take 2)")
        default:
            return L("Fino a \(max) caratteri", "Up to \(max) characters")
        }
    }

    /// Allocation units the user may pick (besides "Default"), smallest first. Empty: the
    /// filesystem has a fixed one (F2FS: 4 KiB blocks only).
    var clusterSizes: [UInt32] {
        func powers(_ lo: UInt32, _ hi: UInt32) -> [UInt32] {
            var out: [UInt32] = []
            var v = lo
            while v <= hi { out.append(v); v *= 2 }
            return out
        }
        switch self {
        case .fat16, .fat32: return powers(512, 64 << 10)          // format.rs cluster_limits
        case .exfat: return powers(512, 32 << 20)
        case .ntfs: return powers(512, 2 << 20)
        case .ext2, .ext3, .ext4: return [1024, 2048, 4096]
        case .xfs: return powers(1024, 64 << 10)                   // mkfs/xfs.rs
        case .btrfs, .ufs2: return powers(4096, 64 << 10)          // node size / block size
        case .f2fs: return []
        }
    }

    /// What the allocation unit is called for this filesystem.
    var clusterTitle: String {
        switch self {
        case .fat16, .fat32, .exfat, .ntfs: L("Dimensione cluster", "Cluster size")
        case .btrfs: L("Dimensione nodo", "Node size")
        default: L("Dimensione blocco", "Block size")
        }
    }

    func isValidCluster(_ size: UInt32?) -> Bool {
        guard let size else { return true }
        return clusterSizes.contains(size)
    }
}

extension ForgeScheme {
    var title: String {
        switch self {
        case .gpt: "GPT"
        case .mbr: "MBR"
        case .apm: "APM"
        case .none: L("Nessuna", "None")
        }
    }

    var hint: String {
        switch self {
        case .gpt: L("Consigliato: Mac moderni, PC UEFI, Linux", "Recommended: modern Macs, UEFI PCs, Linux")
        case .mbr: L("PC datati (BIOS), fotocamere, TV, console", "Older PCs (BIOS), cameras, TVs, consoles")
        case .apm: L("Mac PowerPC", "PowerPC Macs")
        case .none: L("Un solo filesystem dall'inizio del disco", "One filesystem from the start of the disk")
        }
    }

    var maxPartitions: Int {
        switch self {
        case .none: 1
        case .mbr: 4
        default: 128
        }
    }
}

extension ForgeWriteMode {
    var title: String {
        switch self {
        case .auto: L("Automatica", "Automatic")
        case .dd: "DD"
        case .iso: "ISO"
        case .windows: "Windows"
        }
    }

    var hint: String {
        switch self {
        case .auto: L("Sceglie LumenDisk in base all'immagine", "LumenDisk decides from the image")
        case .dd: L("Copia byte per byte dell'immagine", "Byte-for-byte copy of the image")
        case .iso: L("Copia i file su una chiavetta resa avviabile", "Copies the files to a stick made bootable")
        case .windows: L("Supporto di installazione di Windows", "Windows installation media")
        }
    }
}

extension WindowsLayout {
    var title: String {
        switch self {
        case .fat32Split: L("FAT32 (install.wim diviso)", "FAT32 (split install.wim)")
        case .ntfsUefi: "NTFS + UEFI:NTFS"
        }
    }
}

extension BootTarget {
    var title: String {
        switch self {
        case .uefi: "UEFI"
        case .bios: "BIOS"
        case .both: L("UEFI e BIOS", "UEFI and BIOS")
        }
    }
}

extension WipeMethod {
    var passes: Int {
        switch self {
        case .zeros, .random: 1
        case .dod3: 3
        case .dod7: 7
        case .gutmann: 35
        }
    }

    var title: String {
        switch self {
        case .zeros: L("Zeri (1 passaggio)", "Zeros (1 pass)")
        case .random: L("Casuale (1 passaggio)", "Random (1 pass)")
        case .dod3: L("DoD 5220.22-M (3 passaggi)", "DoD 5220.22-M (3 passes)")
        case .dod7: L("DoD 5220.22-M ECE (7 passaggi)", "DoD 5220.22-M ECE (7 passes)")
        case .gutmann: L("Gutmann (35 passaggi)", "Gutmann (35 passes)")
        }
    }
}

extension DoctorAction {
    var title: String {
        switch self {
        case .check: L("Controlla", "Check")
        case .repair: L("Ripara", "Repair")
        case .convertToGpt: L("Converti in GPT", "Convert to GPT")
        case .convertToMbr: L("Converti in MBR", "Convert to MBR")
        }
    }
}

// MARK: - Disks

enum ForgeDiskRules {
    /// The only disks the UI lets the user pick: those the core's safety policy allows.
    static func canSelect(_ disk: ForgeDisk) -> Bool {
        disk.allowed && disk.refusal == nil && disk.whole
    }

    /// Allowed disks first, then by BSD number (disk2 before disk10).
    static func sorted(_ disks: [ForgeDisk]) -> [ForgeDisk] {
        disks.sorted { a, b in
            let (sa, sb) = (canSelect(a), canSelect(b))
            if sa != sb { return sa }
            return a.bsd.localizedStandardCompare(b.bsd) == .orderedAscending
        }
    }

    /// Nominal sustained write speed for time estimates, by bus.
    static func nominalWriteSpeed(_ disk: ForgeDisk) -> Double {
        let p = disk.busProtocol.lowercased()
        if disk.diskImage { return 400e6 }
        if p.contains("secure digital") { return 20e6 }
        if p.contains("usb") { return 40e6 }
        if p.contains("thunderbolt") || p.contains("pci") { return 800e6 }
        return 100e6
    }
}

// MARK: - Validation

/// Something that prevents starting (or that the user should fix first).
enum ForgeIssue: Hashable, Sendable {
    case noTarget
    case targetRefused(String)
    case noImage
    case imageLargerThanDisk
    case firstPartitionSize
    case tooManyPartitions
    case checksumInvalid
    case wifiSSIDInvalid
    case wifiCountryRequired
    case wifiCountryInvalid
    case wifiPasswordInvalid
    case hostnameInvalid
    case linuxUserInvalid
    case rpiPasswordRequired
    case sshKeysInvalid
    case windowsAccountEmpty
    case windowsAccountReserved
    case windowsAccountTooLong
    case windowsPasswordTooLong
    case windowsLocaleInvalid
    case answerFileMissing
    case persistenceTooLarge
    case noAlgorithm
    case noHashSource

    var message: String {
        switch self {
        case .noTarget: L("Scegli il disco di destinazione.", "Choose the destination disk.")
        case .targetRefused(let why): L("Questo disco non può essere usato: \(why)", "This disk can't be used: \(why)")
        case .noImage: L("Scegli un'immagine.", "Choose an image.")
        case .imageLargerThanDisk: L("L'immagine è più grande del disco scelto.", "The image is larger than the chosen disk.")
        case .firstPartitionSize: L("La prima partizione deve lasciare spazio per la seconda.", "The first partition must leave room for the second.")
        case .tooManyPartitions: L("Senza tabella delle partizioni si può creare un solo filesystem.", "Without a partition table there can be only one filesystem.")
        case .checksumInvalid: L("Il checksum atteso non è valido: incolla un valore SHA-256, SHA-1 o MD5 in esadecimale.", "The expected checksum is not valid: paste a SHA-256, SHA-1 or MD5 value in hex.")
        case .wifiSSIDInvalid: L("Il nome della rete Wi-Fi (SSID) deve avere da 1 a 32 byte.", "The Wi-Fi network name (SSID) must be 1 to 32 bytes.")
        case .wifiCountryRequired: L("Il Paese del Wi-Fi è obbligatorio (determina i canali consentiti).", "The Wi-Fi country is required (it sets the allowed channels).")
        case .wifiCountryInvalid: L("Il Paese del Wi-Fi è un codice di 2 lettere (es. IT, US).", "The Wi-Fi country is a 2-letter code (e.g. IT, US).")
        case .wifiPasswordInvalid: L("La password WPA deve avere da 8 a 63 caratteri ASCII (oppure 64 cifre esadecimali).", "The WPA password must be 8 to 63 ASCII characters (or 64 hex digits).")
        case .hostnameInvalid: L("Il nome host può contenere solo lettere, cifre e trattini (max 63, non all'inizio o alla fine).", "The hostname may contain only letters, digits and hyphens (max 63, not at the start or end).")
        case .linuxUserInvalid: L("Il nome utente deve iniziare con una lettera minuscola e contenere solo a-z, 0-9, _ e - (max 32, non «root»).", "The user name must start with a lower-case letter and contain only a-z, 0-9, _ and - (max 32, not “root”).")
        case .rpiPasswordRequired: L("Imposta una password per l'utente.", "Set a password for the user.")
        case .sshKeysInvalid: L("Ogni chiave SSH deve essere una riga «ssh-…», «ecdsa-…» o «sk-…».", "Each SSH key must be a line starting with “ssh-”, “ecdsa-” or “sk-”.")
        case .windowsAccountEmpty: L("Scrivi il nome dell'account locale.", "Type the local account name.")
        case .windowsAccountReserved: L("Questo nome è riservato da Windows: scegline un altro.", "Windows reserves this name: choose another one.")
        case .windowsAccountTooLong: L("Il nome dell'account locale può avere al massimo 20 caratteri.", "The local account name can be at most 20 characters.")
        case .windowsPasswordTooLong: L("La password può avere al massimo 127 caratteri.", "The password can be at most 127 characters.")
        case .windowsLocaleInvalid: L("Lingua non valida: usa un codice come it-IT o en-US.", "Invalid locale: use a code like it-IT or en-US.")
        case .answerFileMissing: L("Scegli il file autounattend.xml da usare.", "Choose the autounattend.xml file to use.")
        case .persistenceTooLarge: L("Lo spazio persistente non entra nel disco scelto.", "The persistent storage doesn't fit on the chosen disk.")
        case .noAlgorithm: L("Scegli almeno un algoritmo.", "Choose at least one algorithm.")
        case .noHashSource: L("Scegli un'immagine o un disco.", "Choose an image or a disk.")
        }
    }
}

enum ForgeValidation {
    /// RFC 1123 host label (what Raspberry Pi OS accepts as hostname).
    static func isValidHostname(_ s: String) -> Bool {
        guard (1...63).contains(s.count), !s.hasPrefix("-"), !s.hasSuffix("-") else { return false }
        return s.unicodeScalars.allSatisfy { $0.isASCII && ($0.properties.isAlphabetic || ("0"..."9").contains($0) || $0 == "-") }
    }

    /// Linux user name (Debian `NAME_REGEX` default): [a-z_][a-z0-9_-]{0,31}, not root.
    static func isValidLinuxUser(_ s: String) -> Bool {
        guard (1...32).contains(s.count), s != "root", let first = s.unicodeScalars.first else { return false }
        let lower: (Unicode.Scalar) -> Bool = { ("a"..."z").contains($0) }
        guard lower(first) || first == "_" else { return false }
        return s.unicodeScalars.allSatisfy { lower($0) || ("0"..."9").contains($0) || $0 == "_" || $0 == "-" }
    }

    /// Names Windows reserves for built-in accounts (windows/unattend.rs RESERVED_NAMES).
    static let reservedWindowsNames = ["Administrator", "Järjestelmänvalvoja", "Administrateur", "Rendszergazda", "Administrador",
                                       "Администратор", "Administratör", "Guest", "DefaultAccount", "WDAGUtilityAccount",
                                       "HelpAssistant", "KRBTGT", "Local", "NONE", "SYSTEM"]
    /// Characters Windows doesn't accept in a user name: the core replaces them with "_".
    static let invalidWindowsNameCharacters = Set("/\\[]:;|=.,+*?<>%@&\"")

    static func windowsAccountIssue(_ name: String) -> ForgeIssue? {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        if trimmed.isEmpty { return .windowsAccountEmpty }
        if reservedWindowsNames.contains(where: { $0.lowercased() == trimmed.lowercased() }) { return .windowsAccountReserved }
        if trimmed.count > 20 { return .windowsAccountTooLong }
        return nil
    }

    /// The name Windows will get (invalid characters become "_"), as the core does.
    static func windowsAccountName(_ name: String) -> String {
        String(name.trimmingCharacters(in: .whitespaces).map { invalidWindowsNameCharacters.contains($0) ? "_" : $0 })
    }

    /// A language tag like it-IT, en-US, zh-Hant-TW (unattend.rs `valid_locale`).
    static func isValidWindowsLocale(_ s: String) -> Bool {
        let parts = s.split(separator: "-", omittingEmptySubsequences: false)
        guard let first = parts.first, (2...3).contains(first.count), s.count <= 32,
              first.unicodeScalars.allSatisfy({ $0.isASCII && $0.properties.isAlphabetic }) else { return false }
        return parts.dropFirst().allSatisfy { p in
            (1...8).contains(p.count) && p.unicodeScalars.allSatisfy { $0.isASCII && ($0.properties.isAlphabetic || ("0"..."9").contains($0)) }
        }
    }

    static func isValidSSID(_ s: String) -> Bool { (1...32).contains(s.utf8.count) }

    /// ISO 3166-1 alpha-2.
    static func isValidCountry(_ s: String) -> Bool {
        s.count == 2 && s.unicodeScalars.allSatisfy { $0.isASCII && $0.properties.isAlphabetic }
    }

    /// WPA2/WPA3 personal: empty (open network), 8–63 printable ASCII, or a 64-hex-digit PSK.
    static func isValidWifiPassword(_ s: String) -> Bool {
        if s.isEmpty { return true }
        if s.count == 64 { return s.allSatisfy(\.isHexDigit) }
        return (8...63).contains(s.count) && s.unicodeScalars.allSatisfy { $0.value >= 0x20 && $0.value <= 0x7E }
    }

    static func areValidSSHKeys(_ text: String) -> Bool {
        let lines = text.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty && !$0.hasPrefix("#") }
        return lines.allSatisfy { line in
            ["ssh-", "ecdsa-", "sk-"].contains { line.hasPrefix($0) } && line.split(separator: " ").count >= 2
        }
    }

    /// Normalises a pasted checksum to what the core accepts ("sha256:<hex>"…). Empty input →
    /// .some(nil) (no check); invalid → nil. The algorithm comes from a prefix or the length.
    static func normalizedChecksum(_ raw: String) -> String?? {
        var s = raw.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        if s.isEmpty { return .some(nil) }
        // "SHA256 (file) = <hex>" and "<hex>  file" lines pasted whole.
        if let eq = s.range(of: "= ", options: .backwards), s.hasPrefix("sha") || s.hasPrefix("md5") {
            let algo = s.hasPrefix("sha256") ? "sha256" : s.hasPrefix("sha1") ? "sha1" : s.hasPrefix("md5") ? "md5" : ""
            s = algo + ":" + s[eq.upperBound...].trimmingCharacters(in: .whitespaces)
        } else if let space = s.firstIndex(where: \.isWhitespace) {
            s = String(s[..<space])
        }
        var algo: HashAlgorithm?
        for a in HashAlgorithm.allCases {
            for prefix in [a.rawValue + ":", a.title.lowercased() + ":"] where s.hasPrefix(prefix) {
                algo = a
                s = String(s.dropFirst(prefix.count))
            }
        }
        guard !s.isEmpty, s.allSatisfy(\.isHexDigit) else { return nil }
        let byLength = HashAlgorithm.allCases.first { $0.hexLength == s.count }
        guard let resolved = algo ?? byLength, resolved.hexLength == s.count else { return nil }
        return .some("\(resolved.rawValue):\(s)")
    }
}

// MARK: - Drafts (UI state) → requests

struct PartitionDraft: Sendable, Equatable {
    var fs: ForgeFS = .exfat
    var label = ""
    /// nil = the filesystem's default.
    var clusterSize: UInt32?

    mutating func setFilesystem(_ new: ForgeFS) {
        fs = new
        if !new.isValidCluster(clusterSize) { clusterSize = nil }
    }

    func spec(size: UInt64?) -> PartSpec {
        PartSpec(fs: fs, label: label, clusterSize: fs.isValidCluster(clusterSize) ? clusterSize : nil, size: size, name: nil)
    }
}

enum Choice<T: Hashable & Sendable>: Hashable, Sendable {
    case automatic
    case value(T)

    var value: T? { if case .value(let v) = self { return v }; return nil }
}

struct WindowsDraft: Sendable, Equatable {
    var layout: Choice<WindowsLayout> = .automatic
    var boot: Choice<BootTarget> = .automatic
    var label = ""
    var bypassHardwareChecks = false
    var localAccount = false
    var accountName = ""
    var accountPassword = ""
    var skipPrivacy = false
    var locale = ""
    var timeZone = ""
    var useAnswerFile = false
    var answerFileName: String?
    var answerFileContents: String?

    func issues() -> [ForgeIssue] {
        var out: [ForgeIssue] = []
        if useAnswerFile {
            if answerFileContents == nil { out.append(.answerFileMissing) }
            return out
        }
        if localAccount {
            if let issue = ForgeValidation.windowsAccountIssue(accountName) { out.append(issue) }
            if accountPassword.count > 127 { out.append(.windowsPasswordTooLong) }
        }
        let loc = locale.trimmingCharacters(in: .whitespaces)
        if !loc.isEmpty && !ForgeValidation.isValidWindowsLocale(loc) { out.append(.windowsLocaleInvalid) }
        return out
    }

    func options() -> WindowsOptions {
        var o = WindowsOptions()
        o.layout = layout.value
        o.boot = boot.value
        o.label = label.isEmpty ? nil : label
        if useAnswerFile {
            o.customAnswerFile = answerFileContents
            return o
        }
        o.bypassHardwareChecks = bypassHardwareChecks
        if localAccount {
            o.localAccount = LocalAccount(name: accountName.trimmingCharacters(in: .whitespaces), password: accountPassword)
        }
        o.skipPrivacyQuestions = skipPrivacy
        let loc = locale.trimmingCharacters(in: .whitespaces)
        o.locale = loc.isEmpty ? nil : loc
        let tz = timeZone.trimmingCharacters(in: .whitespaces)
        o.timeZone = tz.isEmpty ? nil : tz
        return o
    }
}

struct LinuxDraft: Sendable, Equatable {
    var persistence = false
    var persistenceSize: UInt64 = 4 << 30
    var fs: Choice<ForgeFS> = .automatic
    var boot: Choice<BootTarget> = .automatic
    var label = ""

    func options() -> LinuxOptions {
        LinuxOptions(persistenceSize: persistence && persistenceSize > 0 ? persistenceSize : nil,
                     fs: fs.value, boot: boot.value, label: label.isEmpty ? nil : label)
    }

    /// Room the persistence partition can take: the disk minus the image and a margin.
    static func persistenceRange(diskSize: UInt64?, imageSize: UInt64?) -> ClosedRange<UInt64>? {
        let lower: UInt64 = 256 << 20
        guard let diskSize else { return nil }
        let used = (imageSize ?? 0) + (64 << 20)
        guard diskSize > used + lower else { return nil }
        return lower...(diskSize - used)
    }
}

struct RpiDraft: Sendable, Equatable {
    var enabled = false
    var hostname = ""
    var user = ""
    var password = ""
    var ssh = false
    var sshKeys = ""
    var wifi = false
    var ssid = ""
    var wifiPassword = ""
    var country = ""
    var hiddenNetwork = false
    var locale = ""
    var timeZone = ""
    var keyboard = ""

    func issues() -> [ForgeIssue] {
        guard enabled else { return [] }
        var out: [ForgeIssue] = []
        let host = hostname.trimmingCharacters(in: .whitespaces)
        if !host.isEmpty && !ForgeValidation.isValidHostname(host) { out.append(.hostnameInvalid) }
        let u = user.trimmingCharacters(in: .whitespaces)
        if !u.isEmpty {
            if !ForgeValidation.isValidLinuxUser(u) { out.append(.linuxUserInvalid) }
            if password.isEmpty && !(ssh && !sshKeys.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty) { out.append(.rpiPasswordRequired) }
        }
        if ssh && !sshKeys.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !ForgeValidation.areValidSSHKeys(sshKeys) {
            out.append(.sshKeysInvalid)
        }
        if wifi {
            if !ForgeValidation.isValidSSID(ssid) { out.append(.wifiSSIDInvalid) }
            let c = country.trimmingCharacters(in: .whitespaces)
            if c.isEmpty { out.append(.wifiCountryRequired) } else if !ForgeValidation.isValidCountry(c) { out.append(.wifiCountryInvalid) }
            if !ForgeValidation.isValidWifiPassword(wifiPassword) { out.append(.wifiPasswordInvalid) }
        }
        return out
    }

    func options() -> RpiOptions {
        func opt(_ s: String) -> String? {
            let t = s.trimmingCharacters(in: .whitespaces)
            return t.isEmpty ? nil : t
        }
        var o = RpiOptions()
        o.hostname = opt(hostname)
        o.user = opt(user)
        o.password = o.user == nil || password.isEmpty ? nil : password
        o.ssh = ssh
        let keys = sshKeys.trimmingCharacters(in: .whitespacesAndNewlines)
        o.sshAuthorizedKeys = ssh && !keys.isEmpty ? keys : nil
        if wifi {
            o.wifi = WifiOptions(ssid: ssid, password: wifiPassword, country: country.trimmingCharacters(in: .whitespaces).uppercased(), hidden: hiddenNetwork)
        }
        o.locale = opt(locale)
        o.timeZone = opt(timeZone)
        o.keyboardLayout = opt(keyboard)
        return o
    }
}

/// Everything the Create & Format screen collects.
struct ForgePlan: Sendable, Equatable {
    // Target
    var targetBSD: String?

    // Format only
    var scheme: ForgeScheme = .gpt
    var primary = PartitionDraft(fs: .exfat, label: "LUMENDISK")
    var addSecond = false
    var primarySize: UInt64 = 8 << 30
    var secondary = PartitionDraft(fs: .ext4, label: "")

    // Image
    var mode: ForgeWriteMode = .auto
    var verify = true
    var expectedChecksum = ""
    var eject = true
    var windows = WindowsDraft()
    var linux = LinuxDraft()
    var rpi = RpiDraft()

    /// The mode the core will use for `inspection` (Auto resolved like flash.rs does).
    func effectiveMode(_ inspection: ImageInspection?) -> ForgeWriteMode {
        mode == .auto ? (inspection?.recommendedMode ?? .dd) : mode
    }

    func formatIssues(disk: ForgeDisk?) -> [ForgeIssue] {
        var out = targetIssues(disk)
        if scheme == .none && addSecond { out.append(.tooManyPartitions) }
        if addSecond, let disk, primarySize == 0 || primarySize + (64 << 20) >= disk.size { out.append(.firstPartitionSize) }
        return out
    }

    func flashIssues(disk: ForgeDisk?, inspection: ImageInspection?) -> [ForgeIssue] {
        var out = targetIssues(disk)
        guard let inspection else { return out + [.noImage] }
        if let disk, let size = inspection.size, size > disk.size { out.append(.imageLargerThanDisk) }
        if ForgeValidation.normalizedChecksum(expectedChecksum) == nil { out.append(.checksumInvalid) }
        let mode = effectiveMode(inspection)
        if mode == .windows { out += windows.issues() }
        if mode == .iso && linux.persistence {
            let range = LinuxDraft.persistenceRange(diskSize: disk?.size, imageSize: inspection.imageSize)
            if let range, !range.contains(linux.persistenceSize) { out.append(.persistenceTooLarge) }
            if range == nil && disk != nil { out.append(.persistenceTooLarge) }
        }
        if mode == .dd { out += rpi.issues() }
        return out
    }

    private func targetIssues(_ disk: ForgeDisk?) -> [ForgeIssue] {
        guard let disk else { return [.noTarget] }
        if !ForgeDiskRules.canSelect(disk) { return [.targetRefused(disk.refusal ?? L("non consentito", "not allowed"))] }
        return []
    }

    /// The format request, or the issues that prevent it.
    func formatRequest(disk: ForgeDisk?) -> Result<ForgeRequest, ForgeIssues> {
        let issues = formatIssues(disk: disk)
        guard issues.isEmpty, let disk else { return .failure(ForgeIssues(issues)) }
        var parts = [primary.spec(size: addSecond ? primarySize : nil)]
        if addSecond && scheme != .none { parts.append(secondary.spec(size: nil)) }
        return .success(.format(FormatRequest(target: .disk(bsd: disk.bsd), scheme: scheme, partitions: parts)))
    }

    func flashRequest(disk: ForgeDisk?, inspection: ImageInspection?) -> Result<ForgeRequest, ForgeIssues> {
        let issues = flashIssues(disk: disk, inspection: inspection)
        guard issues.isEmpty, let disk, let inspection else { return .failure(ForgeIssues(issues)) }
        let mode = effectiveMode(inspection)
        var r = FlashRequest(source: inspection.path, target: .disk(bsd: disk.bsd))
        r.mode = self.mode
        r.verify = verify
        r.expectHash = ForgeValidation.normalizedChecksum(expectedChecksum) ?? nil
        r.eject = eject
        if mode == .windows { r.windows = windows.options() }
        if mode == .iso { r.linux = linux.options() }
        if mode == .dd && rpi.enabled { r.rpi = rpi.options() }
        return .success(.flash(r))
    }
}

struct ForgeIssues: Error, Equatable, Sendable {
    var issues: [ForgeIssue]
    init(_ issues: [ForgeIssue]) { self.issues = issues }
}

// MARK: - Utilities

struct HashDraft: Sendable, Equatable {
    enum Source: Hashable, Sendable {
        case file(String)
        case disk(String)
    }

    var source: Source?
    var algorithms: Set<HashAlgorithm> = [.sha256]
    var decompressed = false
    var compareWith = ""

    func request() -> Result<ForgeRequest, ForgeIssues> {
        var issues: [ForgeIssue] = []
        if source == nil { issues.append(.noHashSource) }
        if algorithms.isEmpty { issues.append(.noAlgorithm) }
        guard issues.isEmpty, let source else { return .failure(ForgeIssues(issues)) }
        let algos = HashAlgorithm.allCases.filter(algorithms.contains)
        switch source {
        case .file(let path):
            return .success(.hash(HashRequest(source: path, algorithms: algos, decompressed: decompressed)))
        case .disk(let bsd):
            return .success(.hash(HashRequest(source: bsd, algorithms: algos)))
        }
    }

    /// Compares the pasted value with the digest of the same algorithm in a hash result:
    /// nil when nothing was pasted or the result has no digest of that kind.
    static func comparison(pasted: String, result: JSONValue?) -> (algorithm: HashAlgorithm, matches: Bool)? {
        guard case .some(.some(let normalized)) = ForgeValidation.normalizedChecksum(pasted), let result else { return nil }
        let parts = normalized.split(separator: ":", maxSplits: 1).map(String.init)
        guard parts.count == 2, let algo = HashAlgorithm(rawValue: parts[0]) else { return nil }
        let candidates = [result[algo.rawValue]?.stringValue, result["decompressed"]?[algo.rawValue]?.stringValue].compactMap { $0?.lowercased() }
        guard !candidates.isEmpty else { return nil }
        return (algo, candidates.contains(parts[1]))
    }
}

enum WipeEstimate {
    /// Seconds to overwrite `size` bytes with `method` (plus one read pass when verifying).
    static func seconds(size: UInt64, method: WipeMethod, verify: Bool, bytesPerSecond: Double) -> Double {
        guard bytesPerSecond > 0 else { return 0 }
        let passes = Double(method.passes + (verify ? 1 : 0))
        return Double(size) * passes / bytesPerSecond
    }
}

// MARK: - Formatting

enum ForgeFormat {
    static func fraction(done: UInt64, total: UInt64) -> Double? {
        guard total > 0 else { return nil }
        return min(Double(done) / Double(total), 1)
    }

    private static func number(_ value: Double, digits: Int, locale: Locale) -> String {
        value.formatted(.number.precision(.fractionLength(digits)).locale(locale).grouping(.automatic))
    }

    /// Localised name of a progress unit ("files", "partitions", "steps", "passes").
    static func unitName(_ unit: String, count: Double = 2) -> String {
        let one = count == 1
        switch unit {
        case "files": return one ? "file" : L("file", "files")
        case "partitions": return one ? L("partizione", "partition") : L("partizioni", "partitions")
        case "steps": return one ? L("passo", "step") : L("passi", "steps")
        case "passes": return one ? L("passaggio", "pass") : L("passaggi", "passes")
        default: return unit
        }
    }

    /// "123,4 MB/s" for bytes (decimal units, as Finder), "3,2 file/s" for other units.
    static func rate(_ rate: Double, unit: String = "bytes", locale: Locale = L10n.locale) -> String {
        guard rate.isFinite, rate > 0 else { return "—" }
        if unit != "bytes" {
            return number(rate, digits: rate < 10 ? 1 : 0, locale: locale) + " \(unitName(unit))/s"
        }
        let steps: [(Double, String)] = [(1e9, "GB/s"), (1e6, "MB/s"), (1e3, "kB/s")]
        for (scale, name) in steps where rate >= scale {
            let v = rate / scale
            return number(v, digits: v < 100 ? 1 : 0, locale: locale) + " " + name
        }
        return number(rate, digits: 0, locale: locale) + " B/s"
    }

    /// "45 s", "3 min 05 s", "1 h 02 min"; nil → "—".
    static func duration(_ secs: UInt64?) -> String {
        guard let secs else { return "—" }
        if secs < 60 { return "\(secs) s" }
        if secs < 3600 { return "\(secs / 60) min " + String(format: "%02d s", secs % 60) }
        let h = secs / 3600, m = (secs % 3600) / 60
        if h >= 100 { return "\(h) h" }
        return "\(h) h " + String(format: "%02d min", m)
    }

    static func duration(seconds: Double) -> String {
        guard seconds.isFinite, seconds >= 0 else { return "—" }
        return duration(UInt64(seconds.rounded(.up)))
    }

    /// Stopwatch style: "0:42", "12:05", "1:02:03".
    static func elapsed(_ secs: Double) -> String {
        let s = Int(max(0, secs.isFinite ? secs : 0))
        if s >= 3600 { return String(format: "%d:%02d:%02d", s / 3600, (s % 3600) / 60, s % 60) }
        return String(format: "%d:%02d", s / 60, s % 60)
    }

    /// "1,2 GB di 4 GB", "3 di 7 passi"; total 0 → only what is done.
    static func amount(done: UInt64, total: UInt64, unit: String) -> String {
        if unit == "bytes" {
            if total == 0 { return ByteFormat.string(done) }
            return L("\(ByteFormat.string(done)) di \(ByteFormat.string(total))", "\(ByteFormat.string(done)) of \(ByteFormat.string(total))")
        }
        if total == 0 { return "\(done) \(unitName(unit, count: Double(done)))" }
        let name = unitName(unit, count: Double(total))
        return L("\(done) di \(total) \(name)", "\(done) of \(total) \(name)")
    }

    /// "4 KiB", "512 byte", "32 MiB".
    static func cluster(_ size: UInt32) -> String {
        if size >= 1 << 20 && size % (1 << 20) == 0 { return "\(size >> 20) MiB" }
        if size >= 1 << 10 && size % (1 << 10) == 0 { return "\(size >> 10) KiB" }
        return L("\(size) byte", "\(size) bytes")
    }

    /// "GB/s" style for results expressed in MB/s.
    static func megabytesPerSecond(_ mbs: Double?) -> String {
        guard let mbs else { return "—" }
        return rate(mbs * 1e6)
    }

    /// authopen cancellations become a friendly sentence; other core messages pass as they are
    /// (they are already localised by the core).
    static func friendlyError(_ message: String) -> String {
        let m = message.lowercased()
        let cancelled = ["not authorized", "non autorizzat", "authopen did not return", "authorization was canceled", "-60006"]
        if cancelled.contains(where: { m.contains($0) }) {
            return L("Autorizzazione non concessa: macOS non ha dato a LumenDisk l'accesso al disco, quindi non è stato scritto nulla.",
                     "Authorization not granted: macOS didn't give LumenDisk access to the disk, so nothing was written.")
        }
        return message
    }

    /// The timeline's steps: the phases in the order the job first announced them (a job may
    /// return to an earlier phase, e.g. preparing again after unmounting).
    static func timeline(_ phases: [ForgePhase]) -> [ForgePhase] {
        var seen = Set<ForgePhase>()
        return phases.filter { seen.insert($0).inserted }
    }

    static func phaseTitle(_ phase: ForgePhase) -> String {
        switch phase {
        case .preparing: L("Preparazione", "Preparing")
        case .hashing: L("Checksum", "Hashing")
        case .unmounting: L("Smontaggio", "Unmounting")
        case .wiping: L("Cancellazione", "Wiping")
        case .partitioning: L("Partizionamento", "Partitioning")
        case .formatting: L("Formattazione", "Formatting")
        case .writing: L("Scrittura", "Writing")
        case .extracting: L("Estrazione", "Extracting")
        case .verifying: L("Verifica", "Verifying")
        case .customizing: L("Personalizzazione", "Customizing")
        case .finishing: L("Conclusione", "Finishing")
        case .unknown: L("In corso", "Working")
        }
    }

    static func phaseSymbol(_ phase: ForgePhase) -> String {
        switch phase {
        case .preparing: "gearshape"
        case .hashing: "number"
        case .unmounting: "eject"
        case .wiping: "eraser"
        case .partitioning: "rectangle.split.3x1"
        case .formatting: "square.grid.3x3.fill"
        case .writing: "arrow.down.to.line"
        case .extracting: "archivebox"
        case .verifying: "checkmark.shield"
        case .customizing: "slider.horizontal.3"
        case .finishing: "flag.checkered"
        case .unknown: "ellipsis"
        }
    }
}

// MARK: - Result summaries

/// One line of the result card ("Scritti: 4,2 GB").
struct ForgeSummaryRow: Hashable, Sendable, Identifiable {
    enum Tone: Sendable { case plain, good, bad, mono }
    var title: String
    var value: String
    var tone: Tone = .plain

    var id: String { title + value }
}

enum ForgeSummary {
    static func rows(kind: String, result: JSONValue?) -> [ForgeSummaryRow] {
        guard let r = result else { return [] }
        switch kind {
        case "flash": return flash(r)
        case "format": return format(r)
        case "hash": return hash(r)
        case "doctor": return doctor(r)
        default: return generic(r)
        }
    }

    private static func bytes(_ v: JSONValue?) -> String? { v?.uint64Value.map(ByteFormat.string) }

    static func flash(_ r: JSONValue) -> [ForgeSummaryRow] {
        var rows: [ForgeSummaryRow] = []
        if let mode = r["mode"]?.stringValue { rows.append(.init(title: L("Modalità", "Mode"), value: ForgeWriteMode(rawValue: mode)?.title ?? mode)) }
        if let w = bytes(r["bytes_written"]) { rows.append(.init(title: L("Scritti", "Written"), value: w)) }
        if let s = r["write_mb_s"]?.doubleValue { rows.append(.init(title: L("Velocità di scrittura", "Write speed"), value: ForgeFormat.megabytesPerSecond(s))) }
        if let s = r["verify_mb_s"]?.doubleValue { rows.append(.init(title: L("Velocità di verifica", "Verify speed"), value: ForgeFormat.megabytesPerSecond(s))) }
        if let v = r["verification"] {
            if v["performed"]?.boolValue == true {
                let ok = v["ok"]?.boolValue == true
                rows.append(.init(title: L("Verifica", "Verification"),
                                  value: ok ? L("Superata: il disco corrisponde all'immagine", "Passed: the disk matches the image")
                                            : L("Non superata", "Failed"), tone: ok ? .good : .bad))
            } else {
                rows.append(.init(title: L("Verifica", "Verification"), value: L("Non eseguita", "Not performed")))
            }
        }
        if let sha = r["hashes"]?["image_sha256"]?.stringValue { rows.append(.init(title: "SHA-256", value: sha, tone: .mono)) }
        if let exp = r["hashes"]?["expected"], exp.objectValue != nil {
            let algo = exp["algorithm"]?.stringValue?.uppercased() ?? ""
            rows.append(.init(title: L("Checksum atteso", "Expected checksum"),
                              value: L("\(algo) corrispondente", "\(algo) matched") + (exp["source"]?.stringValue.map { $0 == "request" ? "" : " (\(($0 as NSString).lastPathComponent))" } ?? ""),
                              tone: .good))
        }
        if let total = r["durations"]?["total"]?.doubleValue { rows.append(.init(title: L("Durata", "Duration"), value: ForgeFormat.duration(seconds: total))) }
        if let ejected = r["ejected"]?.boolValue {
            rows.append(.init(title: L("Espulso", "Ejected"), value: ejected ? L("Sì, puoi scollegarlo", "Yes, you can unplug it") : L("No", "No")))
        }
        return rows
    }

    static func format(_ r: JSONValue) -> [ForgeSummaryRow] {
        var rows: [ForgeSummaryRow] = []
        if let s = r["scheme"]?.stringValue { rows.append(.init(title: L("Schema", "Scheme"), value: ForgeScheme(rawValue: s)?.title ?? s)) }
        for p in r["partitions"]?.arrayValue ?? [] {
            let fs = p["fs"]?.stringValue.flatMap(ForgeFS.init(rawValue:))?.displayName ?? p["fs"]?.stringValue ?? "?"
            var value = fs
            if let label = p["label"]?.stringValue, !label.isEmpty { value += " «\(label)»" }
            if let size = bytes(p["size"]) { value += " · " + size }
            if let c = p["cluster_size"]?.uint64Value, c > 0 { value += " · " + ForgeFormat.cluster(UInt32(clamping: c)) }
            rows.append(.init(title: L("Partizione \(p["index"]?.uint64Value ?? 0)", "Partition \(p["index"]?.uint64Value ?? 0)"), value: value))
        }
        if let size = bytes(r["disk_size"]) { rows.append(.init(title: L("Disco", "Disk"), value: size)) }
        return rows
    }

    static func hash(_ r: JSONValue) -> [ForgeSummaryRow] {
        var rows: [ForgeSummaryRow] = []
        if let b = bytes(r["bytes"]) { rows.append(.init(title: L("Letti", "Read"), value: b)) }
        if let s = r["mb_s"]?.doubleValue { rows.append(.init(title: L("Velocità", "Speed"), value: ForgeFormat.megabytesPerSecond(s))) }
        for a in HashAlgorithm.allCases {
            if let h = r[a.rawValue]?.stringValue { rows.append(.init(title: a.title, value: h, tone: .mono)) }
        }
        if let d = r["decompressed"], d.objectValue != nil {
            for a in HashAlgorithm.allCases {
                if let h = d[a.rawValue]?.stringValue { rows.append(.init(title: a.title + " " + L("(decompressa)", "(decompressed)"), value: h, tone: .mono)) }
            }
        }
        if let cf = r["checksum_file"], cf.objectValue != nil {
            let name = (cf["path"]?.stringValue ?? "").split(separator: "/").last.map(String.init) ?? ""
            let matches = cf["matches"]?.boolValue
            rows.append(.init(title: L("File di checksum", "Checksum file"),
                              value: name + " · " + (matches == true ? L("corrisponde", "matches") : matches == false ? L("NON corrisponde", "does NOT match") : L("algoritmo non calcolato", "algorithm not computed")),
                              tone: matches == true ? .good : matches == false ? .bad : .plain))
        }
        return rows
    }

    static func doctor(_ r: JSONValue) -> [ForgeSummaryRow] {
        var rows: [ForgeSummaryRow] = []
        if let actions = r["actions"]?.arrayValue {
            rows.append(.init(title: L("Scritture", "Writes"), value: actions.isEmpty ? L("Nessuna: niente da riparare", "None: nothing to repair")
                                                                                    : actions.compactMap(\.stringValue).joined(separator: ", ")))
        }
        for w in r["warnings"]?.arrayValue?.compactMap(\.stringValue) ?? [] {
            rows.append(.init(title: L("Avviso", "Warning"), value: w, tone: .bad))
        }
        if let report = DoctorReport.from(result: r) {
            rows.append(.init(title: L("Schema", "Scheme"), value: report.scheme.uppercased()))
            rows.append(.init(title: L("Stato", "State"), value: report.healthy ? L("Integra", "Healthy") : L("Problemi trovati", "Problems found"),
                              tone: report.healthy ? .good : .bad))
        }
        return rows
    }

    static func generic(_ r: JSONValue) -> [ForgeSummaryRow] {
        (r.objectValue ?? [:]).sorted { $0.key < $1.key }.compactMap { key, value in
            switch value {
            case .string(let s): ForgeSummaryRow(title: key, value: s)
            case .number(let n): ForgeSummaryRow(title: key, value: n.formatted(.number.locale(L10n.locale)))
            case .bool(let b): ForgeSummaryRow(title: key, value: b ? L("sì", "yes") : L("no", "no"))
            default: nil
            }
        }
    }
}
