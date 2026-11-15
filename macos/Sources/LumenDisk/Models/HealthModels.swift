import Foundation

// MARK: - Phase 3: Disk Health & Speed payloads
//
// Decoded leniently like every other core payload: missing or mistyped fields fall back to
// nil/0 instead of failing the reply.

/// BenchmarkResult (ld_benchmark).
struct BenchmarkResult: Codable, Sendable, Hashable {
    var readMbps: Double = 0
    var writeMbps: Double = 0
    var iops4k: Double = 0
    var latencyMs: Double = 0
    var bytesTested: UInt64 = 0
    var durationMs: UInt64 = 0

    init() {}

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        readMbps = (try? c.decodeIfPresent(Double.self, forKey: .readMbps)) ?? 0
        writeMbps = (try? c.decodeIfPresent(Double.self, forKey: .writeMbps)) ?? 0
        // `convertFromSnakeCase` capitalises the letter after the digit ("iops_4k" → "iops4K");
        // accept every spelling.
        let loose = try decoder.container(keyedBy: LooseKey.self)
        iops4k = ["iops4K", "iops4k", "iops_4k"].lazy
            .compactMap { try? loose.decodeIfPresent(Double.self, forKey: LooseKey($0)) }
            .first ?? 0
        latencyMs = (try? c.decodeIfPresent(Double.self, forKey: .latencyMs)) ?? 0
        bytesTested = (try? c.decodeIfPresent(UInt64.self, forKey: .bytesTested)) ?? 0
        durationMs = (try? c.decodeIfPresent(UInt64.self, forKey: .durationMs)) ?? 0
    }

    private struct LooseKey: CodingKey {
        var stringValue: String
        var intValue: Int? { nil }
        init(_ string: String) { stringValue = string }
        init?(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { nil }
    }

    enum CodingKeys: String, CodingKey {
        case readMbps, writeMbps, iops4k, latencyMs, bytesTested, durationMs
    }

    /// Plain-language verdict based on the slower of the two directions.
    enum Verdict: Sendable {
        case video4K, photos, documents, archives

        init(slowestMBps: Double) {
            switch slowestMBps {
            case 300...: self = .video4K
            case 100...: self = .photos
            case 25...: self = .documents
            default: self = .archives
            }
        }

        var text: String {
            switch self {
            case .video4K: L("Veloce per video 4K", "Fast enough for 4K video")
            case .photos: L("Ottimo per foto e backup", "Great for photos and backups")
            case .documents: L("Adatto a documenti", "Fine for documents")
            case .archives: L("Lento: ideale solo per archivi", "Slow: best for archives")
            }
        }

        var symbol: String {
            switch self {
            case .video4K: "bolt.fill"
            case .photos: "photo.on.rectangle.angled"
            case .documents: "doc.text.fill"
            case .archives: "archivebox.fill"
            }
        }
    }

    var verdict: Verdict { Verdict(slowestMBps: min(readMbps, writeMbps)) }

    // MARK: Rounded, localised numbers

    /// "410 MB/s": tens above 100, units below, one decimal under 10.
    static func speed(_ mbps: Double) -> String {
        let locale = L10n.locale
        let value: String
        switch mbps {
        case 100...: value = grouped(Int((mbps / 10).rounded() * 10))
        case 10...: value = grouped(Int(mbps.rounded()))
        default: value = mbps.formatted(.number.precision(.fractionLength(0...1)).locale(locale))
        }
        return value + " MB/s"
    }

    /// "2.300" / "2,300": hundreds above 1 000, tens above 100.
    static func iops(_ value: Double) -> String {
        let rounded: Double
        switch value {
        case 1000...: rounded = (value / 100).rounded() * 100
        case 100...: rounded = (value / 10).rounded() * 10
        default: rounded = value.rounded()
        }
        return grouped(Int(rounded))
    }

    /// Thousands separator from 1 000 up ("2.300" / "2,300"), even where the locale only groups
    /// from 10 000 (Italian), so speeds and IOPS line up.
    static func grouped(_ value: Int) -> String {
        let separator = L10n.locale.groupingSeparator ?? (L10n.isEnglish ? "," : ".")
        var digits = String(value.magnitude)
        var out = ""
        while digits.count > 3 {
            out = separator + digits.suffix(3) + out
            digits.removeLast(3)
        }
        return (value < 0 ? "-" : "") + digits + out
    }

    /// "0,4 ms" / "12 ms".
    static func latency(_ ms: Double) -> String {
        let locale = L10n.locale
        if ms < 10 {
            return max(ms, 0.1).formatted(.number.precision(.fractionLength(1)).locale(locale)) + " ms"
        }
        return grouped(Int(ms.rounded())) + " ms"
    }

    /// "Lettura 410 MB/s · Scrittura 380 MB/s · Veloce per video 4K".
    var summary: String {
        L("Lettura \(Self.speed(readMbps)) · Scrittura \(Self.speed(writeMbps))",
          "Read \(Self.speed(readMbps)) · Write \(Self.speed(writeMbps))")
    }

    /// "4K casuale: 2.300 IOPS · 0,4 ms".
    var randomSummary: String {
        L("4K casuale: \(Self.iops(iops4k)) IOPS · \(Self.latency(latencyMs))",
          "4K random: \(Self.iops(iops4k)) IOPS · \(Self.latency(latencyMs))")
    }
}

/// DiskHealth (ld_disk_health).
struct DiskHealth: Codable, Sendable, Hashable {
    var bsdName: String = ""
    var mediaName: String?
    var `protocol`: String?
    var solidState: Bool?
    var smartStatus: String?
    var size: UInt64?
    var removable: Bool?
    var `internal`: Bool?
    var location: String?
    var smartctl: SmartctlReport?

    init() {}

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        bsdName = (try? c.decodeIfPresent(String.self, forKey: .bsdName)) ?? ""
        mediaName = (try? c.decodeIfPresent(String.self, forKey: .mediaName))?.trimmingCharacters(in: .whitespaces).nonEmpty
        `protocol` = (try? c.decodeIfPresent(String.self, forKey: .protocol))?.nonEmpty
        solidState = try? c.decodeIfPresent(Bool.self, forKey: .solidState)
        smartStatus = (try? c.decodeIfPresent(String.self, forKey: .smartStatus))?.nonEmpty
        size = try? c.decodeIfPresent(UInt64.self, forKey: .size)
        removable = try? c.decodeIfPresent(Bool.self, forKey: .removable)
        `internal` = try? c.decodeIfPresent(Bool.self, forKey: .internal)
        location = (try? c.decodeIfPresent(String.self, forKey: .location))?.nonEmpty
        smartctl = try? c.decodeIfPresent(SmartctlReport.self, forKey: .smartctl)
    }

    // MARK: Plain language

    enum Connection: Sendable, Hashable {
        case usb, thunderbolt, sdCard, sata, pcie, appleInternal, diskImage, other(String), unknown
    }

    var connection: Connection {
        guard let raw = `protocol`?.lowercased() else { return .unknown }
        if raw.contains("disk image") || raw.contains("virtual") { return .diskImage }
        if raw.contains("usb") { return .usb }
        if raw.contains("thunderbolt") { return .thunderbolt }
        if raw.contains("secure digital") || raw == "sd" || raw.contains("sdxc") || raw.contains("sdhc") { return .sdCard }
        if raw.contains("sata") || raw.contains("ata") { return .sata }
        if raw.contains("pci") || raw.contains("nvme") { return .pcie }
        if raw.contains("apple fabric") { return .appleInternal }
        return .other(`protocol` ?? "")
    }

    var isDiskImage: Bool { connection == .diskImage }

    var connectionText: String {
        switch connection {
        case .usb: "USB"
        case .thunderbolt: "Thunderbolt"
        case .sdCard: L("Scheda SD", "SD card")
        case .sata: "SATA"
        case .pcie: "PCIe"
        case .appleInternal: L("Interno (Apple)", "Internal (Apple)")
        case .diskImage: L("Immagine disco collegata", "Attached disk image")
        case .other(let name): name
        case .unknown: L("Non indicato", "Not reported")
        }
    }

    var connectionSymbol: String {
        switch connection {
        case .usb: "cable.connector"
        case .thunderbolt: "bolt.horizontal.fill"
        case .sdCard: "sdcard.fill"
        case .sata, .pcie, .appleInternal: "internaldrive.fill"
        case .diskImage: "doc.fill"
        case .other, .unknown: "cable.connector"
        }
    }

    var mediaTypeText: String {
        if isDiskImage { return L("Virtuale (file su questo Mac)", "Virtual (a file on this Mac)") }
        switch solidState {
        case true?: return L("SSD · memoria flash", "SSD · flash storage")
        case false?: return L("Disco rigido · meccanico", "Hard disk · spinning")
        case nil: return L("Non indicato", "Not reported")
        }
    }

    var mediaTypeSymbol: String {
        if isDiskImage { return "doc.on.doc.fill" }
        return solidState == false ? "opticaldiscdrive.fill" : "memorychip.fill"
    }

    enum SMARTState: Sendable { case verified, failing, unavailable }

    var smartState: SMARTState {
        switch smartStatus?.lowercased() {
        case "verified"?: .verified
        case "failing"?: .failing
        default: smartctl?.passed == false ? .failing : (smartctl?.passed == true ? .verified : .unavailable)
        }
    }

    var smartText: String {
        switch smartState {
        case .verified: return L("Verificato · nessun problema", "Verified · no problems")
        case .failing: return L("Attenzione: il disco segnala guasti", "Warning: the disk reports failures")
        case .unavailable:
            switch connection {
            case .usb: return L("Non disponibile via USB", "Not available over USB")
            case .diskImage: return L("Non applicabile a un'immagine disco", "Not applicable to a disk image")
            default: return L("Non disponibile", "Not available")
            }
        }
    }

    var model: String? {
        let name = smartctl?.modelName ?? mediaName
        if isDiskImage, name?.lowercased() == "disk image" { return nil }
        return name
    }
}

/// The few `smartctl -j -a` facts worth showing, read leniently from the full report.
struct SmartctlReport: Codable, Sendable, Hashable {
    var modelName: String?
    var passed: Bool?
    var temperatureC: Int?
    var powerOnHours: Int?
    var reallocatedSectors: Int?
    var mediaErrors: Int?
    var percentageUsed: Int?

    private struct Temperature: Decodable { var current: Int? }
    private struct PowerOn: Decodable { var hours: Int? }
    private struct Status: Decodable { var passed: Bool? }
    private struct Raw: Decodable { var value: Int? }
    private struct Attribute: Decodable { var id: Int?; var raw: Raw? }
    private struct Attributes: Decodable { var table: [Attribute]? }
    private struct NVMeLog: Decodable {
        var temperature: Int?
        var powerOnHours: Int?
        var mediaErrors: Int?
        var percentageUsed: Int?
    }

    enum CodingKeys: String, CodingKey {
        case modelName, smartStatus, temperature, powerOnTime, ataSmartAttributes, nvmeSmartHealthInformationLog
    }

    init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        modelName = (try? c.decodeIfPresent(String.self, forKey: .modelName))?.trimmingCharacters(in: .whitespaces).nonEmpty
        passed = (try? c.decodeIfPresent(Status.self, forKey: .smartStatus))?.passed
        let nvme = try? c.decodeIfPresent(NVMeLog.self, forKey: .nvmeSmartHealthInformationLog)
        temperatureC = (try? c.decodeIfPresent(Temperature.self, forKey: .temperature))?.current ?? nvme?.temperature
        powerOnHours = (try? c.decodeIfPresent(PowerOn.self, forKey: .powerOnTime))?.hours ?? nvme?.powerOnHours
        let table = (try? c.decodeIfPresent(Attributes.self, forKey: .ataSmartAttributes))?.table ?? []
        reallocatedSectors = table.first { $0.id == 5 }?.raw?.value
        mediaErrors = nvme?.mediaErrors
        percentageUsed = nvme?.percentageUsed
    }

    func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encodeIfPresent(modelName, forKey: .modelName)
    }

    var hasDetails: Bool {
        temperatureC != nil || powerOnHours != nil || reallocatedSectors != nil || mediaErrors != nil || percentageUsed != nil
    }
}
