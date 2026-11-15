// swift-tools-version: 6.0
//
// LumenDisk — native macOS front-end for lumendisk-core.
//
// The Rust core is linked statically from ../build/lib/liblumendisk_core.a (universal),
// produced by the project build script. Set LUMENDISK_LIB_DIR to link another build of the
// library instead (e.g. a development mock); the default is ../build/lib.
import PackageDescription
import Foundation

/// Absolute path of the directory containing this manifest, so that linker search paths do not
/// depend on the current working directory of whoever invokes `swift build`.
let packageDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent().path
let coreLibDir: String = {
    if let custom = ProcessInfo.processInfo.environment["LUMENDISK_LIB_DIR"], !custom.isEmpty {
        return custom.hasPrefix("/") ? custom : "\(packageDir)/\(custom)"
    }
    return "\(packageDir)/../build/lib"
}()
let infoPlist = "\(packageDir)/Resources/Info.plist"

let package = Package(
    name: "LumenDisk",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "LumenDisk", targets: ["LumenDisk"]),
    ],
    targets: [
        // C module exposing core/include/lumendisk_ffi.h to Swift.
        .systemLibrary(
            name: "CLumenDisk",
            path: "Sources/CLumenDisk"
        ),
        .executableTarget(
            name: "LumenDisk",
            dependencies: ["CLumenDisk"],
            path: "Sources/LumenDisk",
            swiftSettings: [
                .enableUpcomingFeature("ExistentialAny"),
            ],
            linkerSettings: [
                .unsafeFlags([
                    "-L", coreLibDir,
                    // Embed Info.plist so a bare `swift run` binary still has a bundle identity.
                    "-Xlinker", "-sectcreate",
                    "-Xlinker", "__TEXT",
                    "-Xlinker", "__info_plist",
                    "-Xlinker", infoPlist,
                ]),
                .linkedLibrary("lumendisk_core"),
                .linkedLibrary("sqlite3"),
                .linkedLibrary("resolv"),
                .linkedLibrary("iconv"),
                .linkedFramework("DiskArbitration"),
                .linkedFramework("Security"),
                .linkedFramework("IOKit"),
                .linkedFramework("SystemConfiguration"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("Network"),
                .linkedFramework("CoreImage"),
            ]
        ),
    ],
    swiftLanguageModes: [.v6]
)
