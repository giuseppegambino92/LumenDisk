import Foundation
import XCTest
@testable import LumenDisk

/// Non-view logic of the Forge screens. Request JSON is compared with golden strings that use
/// the field names of core/src/forge/plan.rs (the ABI), and parsed back to check each field;
/// reply shapes are copied from the Rust side (job.rs, target.rs, image.rs, doctor.rs, the
/// forge section of lumendisk_ffi.h). Nothing here calls the core.
@MainActor
final class ForgeTests: XCTestCase {
    override func setUp() {
        super.setUp()
        MainActor.assumeIsolated { LanguageStore.shared.override(.english) }
    }

    private let en = Locale(identifier: "en_US")
    private let it = Locale(identifier: "it_IT")

    private func disk(_ bsd: String = "disk9", size: UInt64 = 16_000_000_000, allowed: Bool = true, refusal: String? = nil,
                      mounted: [String] = []) -> ForgeDisk {
        ForgeDisk(bsd: bsd, mediaName: "Disk Image", size: size, diskImage: true, busProtocol: "Disk Image",
                  mounted: mounted, allowed: allowed, refusal: refusal)
    }

    private func object(_ json: String) throws -> [String: Any] {
        try XCTUnwrap(JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])
    }

    private func inspection(windows: String = "null", rpi: String = "null", checksum: String = "null", mode: String = "dd") throws -> ImageInspection {
        let json = """
        {"path":"/x.iso","file_name":"x.iso","format":"raw","container":null,"file_size":921600,"compressed":false,
         "size":921600,"size_estimate":null,"random_access":true,"partition_scheme":"MBR",
         "partitions":[{"index":1,"start":0,"size":921600,"scheme":"MBR","type_name":"Unknown","type_id":"0x17","name":""}],
         "iso":{"label":"LUMENLIVE","size":358400,"joliet":true,"udf":false,"el_torito":false,"efi_boot":false,"hybrid_tables":["mbr"],"hybrid":true},
         "windows":\(windows),"linux":null,"rpi":\(rpi),"checksum_file":\(checksum),
         "recommended_mode":"\(mode)","explanation":"Hybrid ISO","notes":[]}
        """
        return try ForgeJSON.decode(ImageInspection.self, from: json)
    }

    // MARK: Request JSON

    func testFormatRequestGoldenJSON() throws {
        var plan = ForgePlan()
        plan.scheme = .gpt
        plan.primary = PartitionDraft(fs: .exfat, label: "DATI", clusterSize: 131072)
        guard case .success(let request) = plan.formatRequest(disk: disk()) else { return XCTFail("format request refused") }
        let json = try request.json()
        // Same request as plan.rs `requests_round_trip_as_the_app_sends_them`.
        XCTAssertEqual(json, #"{"clear_signatures":true,"op":"format","partitions":[{"cluster_size":131072,"fs":"exfat","label":"DATI"}],"scheme":"gpt","target":{"bsd":"disk9","kind":"disk"}}"#)
        let o = try object(json)
        XCTAssertEqual(o["op"] as? String, "format")
        let part = try XCTUnwrap((o["partitions"] as? [[String: Any]])?.first)
        XCTAssertEqual(part["fs"] as? String, "exfat")
        XCTAssertEqual(part["cluster_size"] as? Int, 131072)
        XCTAssertNil(part["size"], "a single partition takes the rest of the disk")
    }

    func testFormatWithTwoPartitionsSizesOnlyTheFirst() throws {
        var plan = ForgePlan()
        plan.scheme = .mbr
        plan.primary = PartitionDraft(fs: .fat32, label: "boot")
        plan.addSecond = true
        plan.primarySize = 512 << 20
        plan.secondary = PartitionDraft(fs: .ext4, label: "rootfs", clusterSize: 4096)
        guard case .success(let request) = plan.formatRequest(disk: disk()) else { return XCTFail() }
        let parts = try XCTUnwrap(try object(request.json())["partitions"] as? [[String: Any]])
        XCTAssertEqual(parts.count, 2)
        XCTAssertEqual(parts[0]["size"] as? Int, 512 << 20)
        XCTAssertEqual(parts[0]["fs"] as? String, "fat32")
        XCTAssertNil(parts[1]["size"])
        XCTAssertEqual(parts[1]["cluster_size"] as? Int, 4096)
        XCTAssertEqual(try object(request.json())["scheme"] as? String, "mbr")

        plan.primarySize = 16_000_000_000
        guard case .failure(let issues) = plan.formatRequest(disk: disk()) else { return XCTFail("partition 1 fills the disk") }
        XCTAssertEqual(issues.issues, [.firstPartitionSize])
        plan.scheme = .none
        plan.primarySize = 1 << 30
        guard case .failure(let issues2) = plan.formatRequest(disk: disk()) else { return XCTFail() }
        XCTAssertTrue(issues2.issues.contains(.tooManyPartitions))
    }

    func testFlashRequestForWindowsMedia() throws {
        let windows = """
        {"version":"Windows 11","arch":"x64","install_image":"/sources/install.wim","install_image_size":5000000000,
          "editions":["Windows 11 Pro"],"build":26100,"server":false,"editions_detail":[],"languages":["it-IT"],
          "image_format":"wim","compression":"lzx","needs_split":true,"split_blocker":null,"bios_boot":true,"uefi_boot":["x64"],
          "recommended_layout":"fat32_split"}
        """
        let insp = try inspection(windows: windows, mode: "windows")
        XCTAssertEqual(insp.windows?.version, "Windows 11")
        XCTAssertEqual(insp.windows?.recommendedLayout, .fat32Split)
        XCTAssertTrue(insp.windows?.needsSplit ?? false)
        var plan = ForgePlan()
        plan.windows.bypassHardwareChecks = true
        plan.windows.localAccount = true
        plan.windows.accountName = "mario"
        plan.rpi.enabled = true   // not used in Windows mode
        guard case .success(let request) = plan.flashRequest(disk: disk(), inspection: insp) else { return XCTFail() }
        let json = try request.json()
        XCTAssertEqual(json, #"{"eject":true,"mode":"auto","op":"flash","source":"/x.iso","target":{"bsd":"disk9","kind":"disk"},"verify":true,"windows":{"bypass_hardware_checks":true,"local_account":{"name":"mario","password":""},"skip_privacy_questions":false}}"#)
        let o = try object(json)
        XCTAssertNil(o["rpi"])
        XCTAssertNil(o["linux"])
        XCTAssertNil(o["expect_hash"])

        plan.windows.layout = .value(.ntfsUefi)
        plan.windows.boot = .value(.uefi)
        plan.windows.locale = "it-IT"
        plan.windows.timeZone = "Europe/Rome"
        plan.windows.skipPrivacy = true
        plan.verify = false
        plan.eject = false
        plan.expectedChecksum = String(repeating: "AB", count: 32)
        guard case .success(let r2) = plan.flashRequest(disk: disk(), inspection: insp) else { return XCTFail() }
        let o2 = try object(r2.json())
        let w = try XCTUnwrap(o2["windows"] as? [String: Any])
        XCTAssertEqual(w["layout"] as? String, "ntfs_uefi")
        XCTAssertEqual(w["boot"] as? String, "uefi")
        XCTAssertEqual(w["locale"] as? String, "it-IT")
        XCTAssertEqual(w["time_zone"] as? String, "Europe/Rome")
        XCTAssertEqual(w["skip_privacy_questions"] as? Bool, true)
        XCTAssertEqual(o2["verify"] as? Bool, false)
        XCTAssertEqual(o2["eject"] as? Bool, false)
        XCTAssertEqual(o2["expect_hash"] as? String, "sha256:" + String(repeating: "ab", count: 32))

        // A custom answer file replaces every other unattended option.
        plan.windows.useAnswerFile = true
        plan.windows.answerFileContents = "<unattend/>"
        guard case .success(let r3) = plan.flashRequest(disk: disk(), inspection: insp) else { return XCTFail() }
        let w3 = try XCTUnwrap(try object(r3.json())["windows"] as? [String: Any])
        XCTAssertEqual(w3["custom_answer_file"] as? String, "<unattend/>")
        XCTAssertNil(w3["local_account"])
        XCTAssertEqual(w3["bypass_hardware_checks"] as? Bool, false)
    }

    func testFlashRequestForRaspberryPiAndLinux() throws {
        let rpi = try inspection(rpi: #"{"os":"Raspberry Pi OS (bookworm)","method":"firstrun"}"#)
        XCTAssertEqual(rpi.rpi?.method, "firstrun")
        XCTAssertEqual(rpi.recommendedMode, .dd)
        var plan = ForgePlan()
        plan.rpi.enabled = true
        plan.rpi.hostname = "lumenpi"
        plan.rpi.user = "mario"
        plan.rpi.password = "secret"
        plan.rpi.ssh = true
        plan.rpi.wifi = true
        plan.rpi.ssid = "Casa"
        plan.rpi.wifiPassword = "password123"
        plan.rpi.country = "it"
        guard case .success(let request) = plan.flashRequest(disk: disk(), inspection: rpi) else { return XCTFail() }
        let o = try object(request.json())
        let r = try XCTUnwrap(o["rpi"] as? [String: Any])
        XCTAssertEqual(try ForgeJSON.encode(plan.rpi.options()),
                       #"{"hostname":"lumenpi","password":"secret","ssh":true,"user":"mario","wifi":{"country":"IT","hidden":false,"password":"password123","ssid":"Casa"}}"#)
        XCTAssertEqual(r["hostname"] as? String, "lumenpi")
        XCTAssertNil(o["windows"])

        // ISO mode: Linux options (persistence in bytes, FS, boot).
        plan.mode = .iso
        plan.linux.persistence = true
        plan.linux.persistenceSize = 1 << 30
        plan.linux.fs = .value(.exfat)
        plan.linux.boot = .value(.both)
        guard case .success(let iso) = plan.flashRequest(disk: disk(), inspection: rpi) else { return XCTFail() }
        let o2 = try object(iso.json())
        XCTAssertEqual(o2["mode"] as? String, "iso")
        let l = try XCTUnwrap(o2["linux"] as? [String: Any])
        XCTAssertEqual(l["persistence_size"] as? Int, 1 << 30)
        XCTAssertEqual(l["fs"] as? String, "exfat")
        XCTAssertEqual(l["boot"] as? String, "both")
        XCTAssertNil(o2["rpi"], "Raspberry Pi customisation only follows a DD write")
    }

    func testUtilityRequests() throws {
        var hash = HashDraft()
        hash.source = .file("/images/a.img.xz")
        hash.algorithms = [.md5, .sha256]
        hash.decompressed = true
        guard case .success(let h) = hash.request() else { return XCTFail() }
        XCTAssertEqual(try h.json(), #"{"algorithms":["sha256","md5"],"decompressed":true,"op":"hash","source":"/images/a.img.xz"}"#)
        hash.source = .disk("disk9")
        guard case .success(let hd) = hash.request() else { return XCTFail() }
        XCTAssertEqual(try object(hd.json())["source"] as? String, "disk9")
        XCTAssertEqual(try object(hd.json())["decompressed"] as? Bool, false)
        XCTAssertEqual(hd.diskBSD, "disk9")
        hash.algorithms = []
        hash.source = nil
        guard case .failure(let issues) = hash.request() else { return XCTFail() }
        XCTAssertEqual(issues.issues, [.noHashSource, .noAlgorithm])

        let wipe = ForgeRequest.wipe(WipeRequest(target: .disk(bsd: "disk9"), method: .dod3, verify: true))
        XCTAssertEqual(try wipe.json(), #"{"method":"dod3","op":"wipe","target":{"bsd":"disk9","kind":"disk"},"verify":true}"#)
        let doctor = ForgeRequest.doctor(DoctorRequest(target: .disk(bsd: "disk9"), action: .convertToGpt))
        XCTAssertEqual(try doctor.json(), #"{"action":"convert_to_gpt","op":"doctor","target":{"bsd":"disk9","kind":"disk"}}"#)
        XCTAssertEqual(DoctorAction.allCases.map(\.rawValue), ["check", "repair", "convert_to_gpt", "convert_to_mbr"])
        XCTAssertEqual(WipeMethod.allCases.map(\.rawValue), ["zeros", "random", "dod3", "dod7", "gutmann"])
        let file = ForgeRequest.format(FormatRequest(target: .file(path: "/tmp/o.img", size: nil), scheme: .none,
                                                    partitions: [PartSpec(fs: .ufs2, label: "x")]))
        XCTAssertEqual(try file.json(), #"{"clear_signatures":true,"op":"format","partitions":[{"fs":"ufs2","label":"x"}],"scheme":"none","target":{"kind":"file","path":"/tmp/o.img","size":null}}"#)
        XCTAssertEqual(ForgeFS.allCases.map(\.rawValue), ["ext2", "ext3", "ext4", "fat16", "fat32", "exfat", "ntfs", "btrfs", "xfs", "f2fs", "ufs2"])
    }

    // MARK: Reply decoding

    func testSnapshotDecoding() throws {
        let json = """
        {"ok":true,"id":7,"kind":"flash","state":"running","phase":"writing","phases":["preparing","hashing","unmounting","writing"],
         "done":104857600,"total":419430400,"unit":"bytes","rate":52428800.5,"eta_secs":6,"elapsed_secs":12.25,
         "logs":[{"seq":4,"ms":1200,"level":"info","msg":"disk9 opened for writing"},{"seq":5,"ms":1300,"level":"warn","msg":"slow"}],
         "result":null,"error":null}
        """
        let s = try ForgeJSON.decode(ForgeSnapshot.self, from: json)
        XCTAssertEqual(s.id, 7)
        XCTAssertEqual(s.state, .running)
        XCTAssertEqual(s.phase, .writing)
        XCTAssertEqual(s.phases, [.preparing, .hashing, .unmounting, .writing])
        XCTAssertEqual(s.fraction ?? 0, 0.25, accuracy: 1e-9)
        XCTAssertEqual(s.etaSecs, 6)
        XCTAssertEqual(s.logs.map(\.seq), [4, 5])
        XCTAssertEqual(s.logs[1].level, .warn)
        XCTAssertNil(s.result)

        let done = """
        {"ok":true,"id":8,"kind":"format","state":"succeeded","phase":"finishing","phases":["preparing","unmounting","preparing","partitioning","formatting","finishing","future_phase"],
         "done":0,"total":0,"unit":"partitions","rate":0.0,"eta_secs":null,"elapsed_secs":0.8,"logs":[],
         "result":{"target":"/dev/rdisk9","scheme":"gpt","sector_size":512,"disk_size":268435456,
                   "partitions":[{"index":1,"start":1048576,"size":267369984,"fs":"exfat","label":"LUMENTEST","name":"LUMENTEST","cluster_size":4096}]},
         "error":null}
        """
        let d = try ForgeJSON.decode(ForgeSnapshot.self, from: done)
        XCTAssertEqual(d.state, .succeeded)
        XCTAssertNil(d.fraction, "total 0 = indeterminate")
        XCTAssertEqual(d.phases.last, .unknown, "unknown phases don't break decoding")
        XCTAssertEqual(d.result?["partitions"]?.arrayValue?.first?["label"]?.stringValue, "LUMENTEST")
        XCTAssertEqual(ForgeFormat.timeline(d.phases), [.preparing, .unmounting, .partitioning, .formatting, .finishing, .unknown])
        let rows = ForgeSummary.rows(kind: "format", result: d.result)
        XCTAssertEqual(rows.first?.value, "GPT")
        XCTAssertTrue(rows.contains { $0.value.contains("exFAT «LUMENTEST»") && $0.value.contains("4 KiB") })

        let failed = #"{"ok":true,"id":9,"kind":"wipe","state":"failed","phase":"preparing","phases":["preparing"],"done":0,"total":0,"unit":"bytes","rate":0.0,"eta_secs":null,"elapsed_secs":0.01,"logs":[{"seq":1,"ms":3,"level":"error","msg":"wipe: not implemented yet"}],"result":null,"error":"wipe: not implemented yet"}"#
        let f = try ForgeJSON.decode(ForgeSnapshot.self, from: failed)
        XCTAssertEqual(f.state, .failed)
        XCTAssertEqual(f.error, "wipe: not implemented yet")
        XCTAssertEqual(f.logs.first?.level, .error)
    }

    func testFlashAndHashResultSummaries() throws {
        let flash = """
        {"mode":"dd","source":"/x.img","format":"xz","target":"/dev/rdisk9","image_size":4000000000,"bytes_written":4000000512,
         "write_mb_s":41.5,"verify_mb_s":90.25,
         "verification":{"performed":true,"ok":true,"method":"sha256","target_sha256":"aa","first_mismatch":null},
         "hashes":{"image_sha256":"\(String(repeating: "0", count: 64))","expected":{"algorithm":"sha256","hex":"00","source":"/d/SHA256SUMS","matched":"decompressed"}},
         "ejected":true,"durations":{"hashing":1.0,"writing":96.4,"verifying":44.3,"total":142.0}}
        """
        let result = try ForgeJSON.decode(JSONValue.self, from: flash)
        let rows = ForgeSummary.rows(kind: "flash", result: result)
        XCTAssertTrue(rows.contains { $0.title == "Written" && $0.value == ByteFormat.string(UInt64(4_000_000_512)) })
        XCTAssertTrue(rows.contains { $0.title == "Write speed" && $0.value == "41.5 MB/s" })
        XCTAssertTrue(rows.contains { $0.title == "Verification" && $0.tone == .good })
        XCTAssertTrue(rows.contains { $0.title == "Expected checksum" && $0.value.contains("SHA256 matched (SHA256SUMS)") })
        XCTAssertTrue(rows.contains { $0.title == "Ejected" && $0.value.hasPrefix("Yes") })
        XCTAssertTrue(rows.contains { $0.title == "Duration" && $0.value == "2 min 22 s" })

        let hash = """
        {"source":"/x.img","kind":"file","what":"file","offset":0,"bytes":1048576,"sha256":"\(String(repeating: "ab", count: 32))","md5":"\(String(repeating: "cd", count: 16))",
         "decompressed":null,"checksum_file":{"path":"/d/x.img.sha256","algorithm":"sha256","expected":"ab","matches":false},"seconds":0.01,"mb_s":104.8}
        """
        let h = try ForgeJSON.decode(JSONValue.self, from: hash)
        let hr = ForgeSummary.rows(kind: "hash", result: h)
        XCTAssertTrue(hr.contains { $0.title == "SHA-256" && $0.tone == .mono })
        XCTAssertFalse(hr.contains { $0.title == "SHA-1" })
        XCTAssertTrue(hr.contains { $0.title == "Checksum file" && $0.tone == .bad && $0.value.hasPrefix("x.img.sha256") })
        let same = HashDraft.comparison(pasted: "SHA256:" + String(repeating: "AB", count: 32), result: h)
        XCTAssertEqual(same?.algorithm, .sha256)
        XCTAssertEqual(same?.matches, true)
        XCTAssertEqual(HashDraft.comparison(pasted: String(repeating: "ce", count: 16), result: h)?.matches, false)
        XCTAssertNil(HashDraft.comparison(pasted: String(repeating: "ab", count: 20), result: h), "no SHA-1 was computed")
        XCTAssertNil(HashDraft.comparison(pasted: "", result: h))
    }

    func testDiskAndInspectionDecoding() throws {
        let json = """
        {"ok":true,"disks":[
          {"bsd":"disk4","media_name":"USB Flash Drive","size":62008590336,"block_size":512,"internal":false,"removable":true,"ejectable":true,
           "whole":true,"writable":true,"disk_image":false,"protocol":"USB","boot":false,"mounted":["/Volumes/ESD-USB"],"allowed":false,
           "refusal":"test mode: only attached disk images are allowed"},
          {"bsd":"disk15","media_name":"Disk Image","size":268435456,"block_size":512,"internal":false,"removable":false,"ejectable":true,
           "whole":true,"writable":true,"disk_image":true,"protocol":"Disk Image","boot":false,"mounted":[],"allowed":true,"refusal":null}]}
        """
        let disks = try ForgeJSON.decode(ForgeDisksReply.self, from: json).disks
        XCTAssertEqual(disks.count, 2)
        XCTAssertEqual(disks[0].busProtocol, "USB")
        XCTAssertEqual(disks[0].volumeNames, ["ESD-USB"])
        XCTAssertFalse(ForgeDiskRules.canSelect(disks[0]))
        XCTAssertTrue(ForgeDiskRules.canSelect(disks[1]))
        XCTAssertEqual(disks[1].title, "Attached disk image")
        XCTAssertEqual(ForgeDiskRules.sorted(disks).map(\.bsd), ["disk15", "disk4"], "allowed disks first")

        let insp = try inspection(checksum: #"{"path":"/d/SHA256SUMS","algorithm":"sha256","expected":"ab"}"#)
        XCTAssertEqual(insp.recommendedMode, .dd)
        XCTAssertEqual(insp.iso?.hybrid, true)
        XCTAssertEqual(insp.iso?.hybridTables, ["mbr"])
        XCTAssertEqual(insp.partitions.first?.typeName, "Unknown")
        XCTAssertEqual(insp.checksumFile?.algorithm, "sha256")
        XCTAssertEqual(insp.imageSize, 921600)
        // A compressed image of unknown size: the estimate is used for limits.
        let xz = try ForgeJSON.decode(ImageInspection.self, from: #"{"path":"/a.img.xz","format":"xz","file_size":10,"compressed":true,"size":null,"size_estimate":4000,"random_access":false,"partitions":[],"recommended_mode":"dd","explanation":"","notes":["partitions: needs decompression"]}"#)
        XCTAssertNil(xz.size)
        XCTAssertEqual(xz.imageSize, 4000)
        XCTAssertEqual(xz.notes.count, 1)
    }

    func testDoctorReportDecoding() throws {
        let check = """
        {"scheme":"gpt","sector_size":512,"disk_size":268435456,
         "partitions":[{"index":1,"start":1048576,"size":267369984,"type":"EBD0A0A2-B9E5-4433-87C0-68B6B72699C7","name":"DATA"}],
         "findings":[{"code":"gpt_backup_header_crc","severity":"error","message":"backup GPT header damaged","repairable":true},
                     {"code":"pmbr_size","severity":"warning","message":"protective MBR size","repairable":true}],
         "healthy":false,"repairable":true}
        """
        let r = try XCTUnwrap(DoctorReport.from(result: try ForgeJSON.decode(JSONValue.self, from: check)))
        XCTAssertEqual(r.findings.count, 2)
        XCTAssertEqual(r.findings[0].severity, .error)
        XCTAssertTrue(r.findings[0].repairable)
        XCTAssertGreaterThan(DoctorSeverity.error, DoctorSeverity.warning)
        XCTAssertFalse(r.healthy)
        // Repair / convert results carry the report after writing.
        let repair = #"{"action":"repair","actions":["backup GPT header"],"warnings":[],"before":\#(check),"after":{"scheme":"gpt","sector_size":512,"disk_size":268435456,"partitions":[],"findings":[],"healthy":true,"repairable":false}}"#
        let rv = try ForgeJSON.decode(JSONValue.self, from: repair)
        XCTAssertEqual(DoctorReport.from(result: rv)?.healthy, true)
        let rows = ForgeSummary.rows(kind: "doctor", result: rv)
        XCTAssertTrue(rows.contains { $0.title == "Writes" && $0.value == "backup GPT header" })
        XCTAssertTrue(rows.contains { $0.title == "State" && $0.value == "Healthy" })
    }

    // MARK: Formatting

    func testRateDurationAndAmountFormatting() {
        XCTAssertEqual(ForgeFormat.rate(12_345_678, locale: en), "12.3 MB/s")
        XCTAssertEqual(ForgeFormat.rate(12_345_678, locale: it), "12,3 MB/s")
        XCTAssertEqual(ForgeFormat.rate(123_456_789, locale: en), "123 MB/s")
        XCTAssertEqual(ForgeFormat.rate(1_500_000_000, locale: en), "1.5 GB/s")
        XCTAssertEqual(ForgeFormat.rate(2_500, locale: en), "2.5 kB/s")
        XCTAssertEqual(ForgeFormat.rate(999, locale: en), "999 B/s")
        XCTAssertEqual(ForgeFormat.rate(0, locale: en), "—")
        XCTAssertEqual(ForgeFormat.rate(.nan, locale: en), "—")
        XCTAssertEqual(ForgeFormat.rate(3, unit: "files", locale: en), "3.0 files/s")
        XCTAssertEqual(ForgeFormat.rate(42, unit: "steps", locale: en), "42 steps/s")

        XCTAssertEqual(ForgeFormat.duration(nil), "—")
        XCTAssertEqual(ForgeFormat.duration(45), "45 s")
        XCTAssertEqual(ForgeFormat.duration(185), "3 min 05 s")
        XCTAssertEqual(ForgeFormat.duration(3720), "1 h 02 min")
        XCTAssertEqual(ForgeFormat.duration(seconds: 0.2), "1 s")
        XCTAssertEqual(ForgeFormat.elapsed(42.7), "0:42")
        XCTAssertEqual(ForgeFormat.elapsed(725), "12:05")
        XCTAssertEqual(ForgeFormat.elapsed(3723), "1:02:03")

        XCTAssertEqual(ForgeFormat.amount(done: 3, total: 7, unit: "steps"), "3 of 7 steps")
        XCTAssertEqual(ForgeFormat.amount(done: 1, total: 0, unit: "partitions"), "1 partition")
        XCTAssertEqual(ForgeFormat.amount(done: 1_000_000, total: 4_000_000, unit: "bytes"),
                       "\(ByteFormat.string(UInt64(1_000_000))) of \(ByteFormat.string(UInt64(4_000_000)))")
        XCTAssertEqual(ForgeFormat.fraction(done: 5, total: 0), nil)
        XCTAssertEqual(ForgeFormat.fraction(done: 50, total: 40), 1)
        XCTAssertEqual(ForgeFormat.cluster(512), "512 bytes")
        XCTAssertEqual(ForgeFormat.cluster(4096), "4 KiB")
        XCTAssertEqual(ForgeFormat.cluster(32 << 20), "32 MiB")
        XCTAssertEqual(ForgeFormat.megabytesPerSecond(41.5), "41.5 MB/s")
    }

    func testFriendlyAuthorizationErrors() {
        let friendly = ForgeFormat.friendlyError("/dev/rdisk4: authopen did not return a descriptor (exit status: 1)")
        XCTAssertTrue(friendly.hasPrefix("Authorization not granted"))
        XCTAssertTrue(ForgeFormat.friendlyError("Not authorized").hasPrefix("Authorization not granted"))
        XCTAssertEqual(ForgeFormat.friendlyError("the media is write-protected"), "the media is write-protected")
    }

    // MARK: Filesystem rules

    func testClusterSizesPerFilesystem() {
        XCTAssertEqual(ForgeFS.fat32.clusterSizes, [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536])
        XCTAssertEqual(ForgeFS.fat16.clusterSizes.last, 65536)
        XCTAssertEqual(ForgeFS.exfat.clusterSizes.first, 512)
        XCTAssertEqual(ForgeFS.exfat.clusterSizes.last, 32 << 20)
        XCTAssertEqual(ForgeFS.ntfs.clusterSizes.last, 2 << 20)
        XCTAssertEqual(ForgeFS.ext4.clusterSizes, [1024, 2048, 4096])
        XCTAssertEqual(ForgeFS.ext2.clusterSizes, ForgeFS.ext3.clusterSizes)
        XCTAssertEqual(ForgeFS.xfs.clusterSizes, [1024, 2048, 4096, 8192, 16384, 32768, 65536])
        XCTAssertEqual(ForgeFS.btrfs.clusterSizes, [4096, 8192, 16384, 32768, 65536])
        XCTAssertEqual(ForgeFS.ufs2.clusterSizes.first, 4096)
        XCTAssertTrue(ForgeFS.f2fs.clusterSizes.isEmpty)
        for fs in ForgeFS.allCases {
            XCTAssertTrue(fs.clusterSizes.allSatisfy { $0.nonzeroBitCount == 1 }, "\(fs) sizes are powers of two")
            XCTAssertTrue(fs.isValidCluster(nil))
        }
        var draft = PartitionDraft(fs: .exfat, label: "", clusterSize: 1 << 20)
        draft.setFilesystem(.ntfs)
        XCTAssertEqual(draft.clusterSize, 1 << 20, "1 MiB is valid for NTFS too")
        draft.setFilesystem(.ext4)
        XCTAssertNil(draft.clusterSize, "an invalid size falls back to Default")
        XCTAssertNil(PartitionDraft(fs: .f2fs, label: "", clusterSize: 4096).spec(size: nil).clusterSize)
    }

    func testLabelRules() {
        XCTAssertEqual(ForgeFS.fat32.sanitizedLabel("my disk/2024!"), "MY DISK2024")
        XCTAssertEqual(ForgeFS.fat16.sanitizedLabel("àbc"), "BC")
        XCTAssertEqual(ForgeFS.fat32.sanitizedLabel("AB   "), "AB")
        XCTAssertEqual(ForgeFS.fat32.labelLimit.max, 11)
        XCTAssertEqual(ForgeFS.exfat.sanitizedLabel("Fotografie2024"), "Fotografie2")
        XCTAssertEqual(ForgeFS.exfat.sanitizedLabel("📷Photos"), "📷Photos")
        XCTAssertEqual(ForgeFS.exfat.sanitizedLabel(String(repeating: "📷", count: 6)), String(repeating: "📷", count: 5), "UTF-16 units")
        XCTAssertEqual(ForgeFS.ext4.sanitizedLabel(String(repeating: "à", count: 10)), String(repeating: "à", count: 8), "16 UTF-8 bytes")
        XCTAssertEqual(ForgeFS.ufs2.sanitizedLabel("my disk.1"), "my_disk_1")
        XCTAssertEqual(ForgeFS.ntfs.sanitizedLabel(String(repeating: "n", count: 40)).count, 32)
        XCTAssertEqual(ForgeFS.xfs.sanitizedLabel("abcdefghijklmnop"), "abcdefghijkl")
        XCTAssertEqual(ForgeFS.btrfs.sanitizedLabel("A\u{7}B"), "AB", "control characters are dropped")
        XCTAssertEqual(ForgeFS.ext4.labelLength("àb"), 3)
        XCTAssertEqual(ForgeFS.exfat.labelLength("📷"), 2)
    }

    // MARK: Validation

    func testRaspberryPiValidation() {
        var r = RpiDraft()
        r.enabled = true
        r.wifi = true
        r.ssid = "Casa"
        r.wifiPassword = "password123"
        XCTAssertEqual(r.issues(), [.wifiCountryRequired])
        r.country = "ITA"
        XCTAssertEqual(r.issues(), [.wifiCountryInvalid])
        r.country = "it"
        XCTAssertEqual(r.issues(), [])
        r.wifiPassword = "short"
        XCTAssertEqual(r.issues(), [.wifiPasswordInvalid])
        r.wifiPassword = ""
        XCTAssertEqual(r.issues(), [], "an open network has no password")
        r.ssid = String(repeating: "s", count: 33)
        XCTAssertEqual(r.issues(), [.wifiSSIDInvalid])
        r.ssid = "Casa"
        r.hostname = "-pi"
        r.user = "Mario"
        XCTAssertEqual(Set(r.issues()), [.hostnameInvalid, .linuxUserInvalid, .rpiPasswordRequired])
        r.hostname = "lumen-pi"
        r.user = "mario"
        r.password = "x"
        r.ssh = true
        r.sshKeys = "not a key"
        XCTAssertEqual(r.issues(), [.sshKeysInvalid])
        r.sshKeys = "ssh-ed25519 AAAAC3Nza mario@mac\n\n# comment\necdsa-sha2-nistp256 AAAAE2 x"
        XCTAssertEqual(r.issues(), [])
        r.enabled = false
        r.hostname = "bad host"
        XCTAssertEqual(r.issues(), [], "nothing is checked while customisation is off")

        XCTAssertTrue(ForgeValidation.isValidWifiPassword(String(repeating: "a1", count: 32)), "64 hex digits = raw PSK")
        XCTAssertFalse(ForgeValidation.isValidWifiPassword(String(repeating: "z", count: 64)))
        XCTAssertFalse(ForgeValidation.isValidWifiPassword(String(repeating: "a", count: 64 + 1)))
        XCTAssertTrue(ForgeValidation.isValidWifiPassword("12345678"))
        XCTAssertFalse(ForgeValidation.isValidWifiPassword("1234567"))
        XCTAssertFalse(ForgeValidation.isValidWifiPassword("pàssword"))
        XCTAssertTrue(ForgeValidation.isValidHostname("raspberrypi"))
        XCTAssertFalse(ForgeValidation.isValidHostname("pi-"))
        XCTAssertFalse(ForgeValidation.isValidHostname("my.pi"))
        XCTAssertFalse(ForgeValidation.isValidHostname(String(repeating: "a", count: 64)))
        XCTAssertTrue(ForgeValidation.isValidLinuxUser("_svc-1"))
        XCTAssertFalse(ForgeValidation.isValidLinuxUser("root"))
        XCTAssertFalse(ForgeValidation.isValidLinuxUser("1user"))
        XCTAssertFalse(ForgeValidation.isValidLinuxUser(String(repeating: "a", count: 33)))
    }

    func testWindowsValidation() {
        XCTAssertEqual(ForgeValidation.windowsAccountIssue("  "), .windowsAccountEmpty)
        XCTAssertEqual(ForgeValidation.windowsAccountIssue("administrator"), .windowsAccountReserved)
        XCTAssertEqual(ForgeValidation.windowsAccountIssue("Guest"), .windowsAccountReserved)
        XCTAssertEqual(ForgeValidation.windowsAccountIssue(String(repeating: "m", count: 21)), .windowsAccountTooLong)
        XCTAssertNil(ForgeValidation.windowsAccountIssue("Mario Rossi"))
        XCTAssertEqual(ForgeValidation.windowsAccountName(" mario.rossi@casa "), "mario_rossi_casa")
        XCTAssertTrue(ForgeValidation.isValidWindowsLocale("it-IT"))
        XCTAssertTrue(ForgeValidation.isValidWindowsLocale("zh-Hant-TW"))
        XCTAssertFalse(ForgeValidation.isValidWindowsLocale("italiano"))
        XCTAssertFalse(ForgeValidation.isValidWindowsLocale("it_IT"))
        var w = WindowsDraft()
        w.localAccount = true
        w.accountName = "Mario"
        w.accountPassword = String(repeating: "p", count: 128)
        w.locale = "xx_YY"
        XCTAssertEqual(w.issues(), [.windowsPasswordTooLong, .windowsLocaleInvalid])
        w.useAnswerFile = true
        XCTAssertEqual(w.issues(), [.answerFileMissing], "with a custom file only the file matters")
    }

    func testChecksumNormalisation() {
        let sha256 = String(repeating: "Ab", count: 32)
        XCTAssertEqual(ForgeValidation.normalizedChecksum(""), .some(nil))
        XCTAssertEqual(ForgeValidation.normalizedChecksum("  \(sha256)\n"), .some("sha256:" + sha256.lowercased()))
        XCTAssertEqual(ForgeValidation.normalizedChecksum("SHA1:" + String(repeating: "1", count: 40)), .some("sha1:" + String(repeating: "1", count: 40)))
        XCTAssertEqual(ForgeValidation.normalizedChecksum("SHA-1:" + String(repeating: "1", count: 40)), .some("sha1:" + String(repeating: "1", count: 40)))
        XCTAssertEqual(ForgeValidation.normalizedChecksum(String(repeating: "f", count: 32)), .some("md5:" + String(repeating: "f", count: 32)))
        XCTAssertEqual(ForgeValidation.normalizedChecksum("\(sha256)  ubuntu.iso"), .some("sha256:" + sha256.lowercased()))
        XCTAssertEqual(ForgeValidation.normalizedChecksum("SHA256 (ubuntu.iso) = \(sha256)"), .some("sha256:" + sha256.lowercased()))
        XCTAssertTrue(ForgeValidation.normalizedChecksum("md5:" + sha256) == nil)
        XCTAssertTrue(ForgeValidation.normalizedChecksum("abc") == nil)
        XCTAssertTrue(ForgeValidation.normalizedChecksum("sha256:" + String(repeating: "g", count: 64)) == nil)
    }

    // MARK: Disks & plan rules

    func testRefusedDiskCannotBeSelected() {
        let refused = disk("disk4", allowed: false, refusal: "it is an internal disk of this Mac")
        let partition = ForgeDisk(bsd: "disk9s1", size: 1, whole: false, allowed: true)
        XCTAssertFalse(ForgeDiskRules.canSelect(refused))
        XCTAssertFalse(ForgeDiskRules.canSelect(partition), "only whole disks")
        XCTAssertFalse(ForgeDiskRules.canSelect(disk("disk5", allowed: true, refusal: "inconsistent verdict")))

        let center = ForgeCenter()
        for tool in ForgeCenter.Tool.allCases {
            XCTAssertFalse(center.select(refused, for: tool))
            XCTAssertNil(center.selectedBSD(for: tool), "\(tool) keeps no target")
        }
        XCTAssertTrue(center.select(disk(), for: .create))
        XCTAssertEqual(center.plan.targetBSD, "disk9")
        XCTAssertFalse(center.select(refused, for: .create))
        XCTAssertEqual(center.plan.targetBSD, "disk9", "a refused click doesn't replace the selection")
        XCTAssertTrue(center.select(disk(), for: .hash))
        XCTAssertEqual(center.hashDraft.source, .disk("disk9"))

        // Even if a refused disk ends up as the target, no request is built for it.
        var plan = ForgePlan()
        guard case .failure(let none) = plan.formatRequest(disk: nil) else { return XCTFail() }
        XCTAssertEqual(none.issues, [.noTarget])
        plan.targetBSD = "disk4"
        guard case .failure(let issues) = plan.formatRequest(disk: refused) else { return XCTFail() }
        XCTAssertEqual(issues.issues, [.targetRefused("it is an internal disk of this Mac")])
    }

    func testFlashPlanIssues() throws {
        let insp = try inspection()
        var plan = ForgePlan()
        XCTAssertEqual(plan.flashIssues(disk: disk(), inspection: nil), [.noImage])
        XCTAssertEqual(plan.flashIssues(disk: disk(size: 1000), inspection: insp), [.imageLargerThanDisk])
        plan.expectedChecksum = "nope"
        XCTAssertEqual(plan.flashIssues(disk: disk(), inspection: insp), [.checksumInvalid])
        plan.expectedChecksum = ""
        XCTAssertEqual(plan.effectiveMode(insp), .dd)
        plan.mode = .iso
        plan.linux.persistence = true
        plan.linux.persistenceSize = 64 << 30
        XCTAssertEqual(plan.flashIssues(disk: disk(), inspection: insp), [.persistenceTooLarge])
        let range = try XCTUnwrap(LinuxDraft.persistenceRange(diskSize: 16_000_000_000, imageSize: 921600))
        XCTAssertEqual(range.lowerBound, 256 << 20)
        XCTAssertEqual(range.upperBound, 16_000_000_000 - 921600 - (64 << 20))
        XCTAssertNil(LinuxDraft.persistenceRange(diskSize: 300 << 20, imageSize: 100 << 20))
        XCTAssertNil(LinuxDraft.persistenceRange(diskSize: nil, imageSize: 0))
    }

    func testWipeEstimates() {
        XCTAssertEqual(WipeMethod.gutmann.passes, 35)
        XCTAssertEqual(WipeMethod.dod7.passes, 7)
        XCTAssertEqual(WipeEstimate.seconds(size: 40_000_000_000, method: .zeros, verify: false, bytesPerSecond: 40e6), 1000)
        XCTAssertEqual(WipeEstimate.seconds(size: 40_000_000_000, method: .dod3, verify: true, bytesPerSecond: 40e6), 4000)
        XCTAssertEqual(WipeEstimate.seconds(size: 1, method: .gutmann, verify: false, bytesPerSecond: 0), 0)
        XCTAssertEqual(ForgeFormat.duration(seconds: WipeEstimate.seconds(size: 64_000_000_000, method: .gutmann, verify: false, bytesPerSecond: 40e6)),
                       "15 h 33 min")
        let usb = ForgeDisk(bsd: "disk4", busProtocol: "USB")
        let sd = ForgeDisk(bsd: "disk5", busProtocol: "Secure Digital")
        XCTAssertLessThan(ForgeDiskRules.nominalWriteSpeed(sd), ForgeDiskRules.nominalWriteSpeed(usb))
    }
}
