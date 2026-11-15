#!/usr/bin/env swift
// Renders the LumenDisk icons with `iconutil`:
//   AppIcon.icns     the app icon (macOS rounded square, soft gradient, drive glyph)
//   VolumeIcon.icns  the Finder icon of every LumenDisk volume (an external-drive glyph on a
//                    soft indigo-to-plum gradient), passed to the core as `volume_icon`
//
// Usage:  swift make_icon.swift [output-directory]
//         (default output directory: the folder containing this script)

import AppKit
import Foundation

let scriptDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let outputDir = CommandLine.arguments.count > 1 ? URL(fileURLWithPath: CommandLine.arguments[1]) : scriptDir

func rgb(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat, _ a: CGFloat = 1) -> CGColor {
    CGColor(srgbRed: r / 255, green: g / 255, blue: b / 255, alpha: a)
}

func linearGradient(_ colors: [CGColor], _ locations: [CGFloat]) -> CGGradient {
    CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB), colors: colors as CFArray, locations: locations)!
}

func roundedRect(_ rect: CGRect, _ radius: CGFloat) -> CGPath {
    CGPath(roundedRect: rect, cornerWidth: radius, cornerHeight: radius, transform: nil)
}

/// Draws the icon in a 1024×1024 coordinate space (origin bottom-left).
func drawIcon(_ cg: CGContext) {
    // Apple icon grid: 824pt body centred in the 1024 canvas.
    let body = CGRect(x: 100, y: 100, width: 824, height: 824)
    let bodyPath = roundedRect(body, 186)

    // Soft drop shadow under the tile.
    cg.saveGState()
    cg.setShadow(offset: CGSize(width: 0, height: -14), blur: 28, color: rgb(0, 0, 0, 0.28))
    cg.addPath(bodyPath)
    cg.setFillColor(rgb(80, 110, 220))
    cg.fillPath()
    cg.restoreGState()

    // Background: calm sky-to-indigo gradient.
    cg.saveGState()
    cg.addPath(bodyPath)
    cg.clip()
    cg.drawLinearGradient(
        linearGradient([rgb(122, 178, 255), rgb(84, 132, 246), rgb(66, 92, 222)], [0, 0.55, 1]),
        start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 100), options: [])
    // Lumen: a soft light bloom from the top-left.
    cg.drawRadialGradient(
        linearGradient([rgb(255, 255, 255, 0.42), rgb(255, 255, 255, 0)], [0, 1]),
        startCenter: CGPoint(x: 330, y: 860), startRadius: 0,
        endCenter: CGPoint(x: 330, y: 860), endRadius: 620, options: [])
    // Subtle top sheen.
    cg.drawLinearGradient(
        linearGradient([rgb(255, 255, 255, 0.16), rgb(255, 255, 255, 0)], [0, 1]),
        start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 640), options: [])
    cg.restoreGState()

    // Hairline inner border.
    cg.saveGState()
    cg.addPath(roundedRect(body.insetBy(dx: 1.5, dy: 1.5), 184.5))
    cg.setStrokeColor(rgb(255, 255, 255, 0.22))
    cg.setLineWidth(3)
    cg.strokePath()
    cg.restoreGState()

    // Drive glyph.
    let drive = CGRect(x: 232, y: 330, width: 560, height: 330)
    let drivePath = roundedRect(drive, 74)

    cg.saveGState()
    cg.setShadow(offset: CGSize(width: 0, height: -22), blur: 40, color: rgb(20, 30, 90, 0.38))
    cg.addPath(drivePath)
    cg.setFillColor(rgb(245, 247, 252))
    cg.fillPath()
    cg.restoreGState()

    cg.saveGState()
    cg.addPath(drivePath)
    cg.clip()
    cg.drawLinearGradient(
        linearGradient([rgb(255, 255, 255), rgb(240, 243, 250), rgb(214, 221, 236)], [0, 0.6, 1]),
        start: CGPoint(x: 512, y: drive.maxY), end: CGPoint(x: 512, y: drive.minY), options: [])

    // Front bezel band.
    let band = CGRect(x: drive.minX, y: drive.minY, width: drive.width, height: 104)
    cg.drawLinearGradient(
        linearGradient([rgb(206, 214, 232), rgb(188, 198, 222)], [0, 1]),
        start: CGPoint(x: 512, y: band.maxY), end: CGPoint(x: 512, y: band.minY), options: [])
    cg.setFillColor(rgb(255, 255, 255, 0.0))
    cg.restoreGState()

    // Re-draw the upper face over the band so the band only shows at the bottom.
    cg.saveGState()
    cg.addPath(drivePath)
    cg.clip()
    let upper = CGRect(x: drive.minX, y: drive.minY + 104, width: drive.width, height: drive.height - 104)
    cg.clip(to: upper)
    cg.drawLinearGradient(
        linearGradient([rgb(255, 255, 255), rgb(244, 246, 252)], [0, 1]),
        start: CGPoint(x: 512, y: drive.maxY), end: CGPoint(x: 512, y: upper.minY), options: [])
    cg.restoreGState()

    // Seam between face and bezel.
    cg.saveGState()
    cg.setStrokeColor(rgb(150, 162, 196, 0.55))
    cg.setLineWidth(3)
    cg.move(to: CGPoint(x: drive.minX + 6, y: drive.minY + 104))
    cg.addLine(to: CGPoint(x: drive.maxX - 6, y: drive.minY + 104))
    cg.strokePath()
    cg.restoreGState()

    // Status LED with a gentle glow.
    let led = CGPoint(x: drive.maxX - 92, y: drive.minY + 52)
    cg.saveGState()
    cg.drawRadialGradient(
        linearGradient([rgb(90, 230, 190, 0.55), rgb(90, 230, 190, 0)], [0, 1]),
        startCenter: led, startRadius: 0, endCenter: led, endRadius: 46, options: [])
    cg.addEllipse(in: CGRect(x: led.x - 15, y: led.y - 15, width: 30, height: 30))
    cg.setFillColor(rgb(52, 211, 160))
    cg.fillPath()
    cg.addEllipse(in: CGRect(x: led.x - 6, y: led.y + 1, width: 10, height: 8))
    cg.setFillColor(rgb(255, 255, 255, 0.7))
    cg.fillPath()
    cg.restoreGState()

    // Vent slots on the bezel.
    cg.saveGState()
    cg.setFillColor(rgb(150, 162, 196, 0.75))
    for i in 0..<4 {
        let x = drive.minX + 70 + CGFloat(i) * 34
        cg.addPath(roundedRect(CGRect(x: x, y: drive.minY + 40, width: 16, height: 26), 8))
    }
    cg.fillPath()
    cg.restoreGState()

    // Top-face highlight line.
    cg.saveGState()
    cg.addPath(roundedRect(drive.insetBy(dx: 2, dy: 2), 72))
    cg.setStrokeColor(rgb(255, 255, 255, 0.9))
    cg.setLineWidth(3)
    cg.strokePath()
    cg.restoreGState()

    // A small four-point sparkle: the "lumen".
    func sparkle(center c: CGPoint, radius r: CGFloat, alpha: CGFloat) {
        let path = CGMutablePath()
        let w = r * 0.22
        path.move(to: CGPoint(x: c.x, y: c.y + r))
        path.addQuadCurve(to: CGPoint(x: c.x + r, y: c.y), control: CGPoint(x: c.x + w, y: c.y + w))
        path.addQuadCurve(to: CGPoint(x: c.x, y: c.y - r), control: CGPoint(x: c.x + w, y: c.y - w))
        path.addQuadCurve(to: CGPoint(x: c.x - r, y: c.y), control: CGPoint(x: c.x - w, y: c.y - w))
        path.addQuadCurve(to: CGPoint(x: c.x, y: c.y + r), control: CGPoint(x: c.x - w, y: c.y + w))
        cg.saveGState()
        cg.setShadow(offset: .zero, blur: 18, color: rgb(255, 255, 255, 0.6 * alpha))
        cg.addPath(path)
        cg.setFillColor(rgb(255, 255, 255, alpha))
        cg.fillPath()
        cg.restoreGState()
    }
    sparkle(center: CGPoint(x: 742, y: 772), radius: 62, alpha: 0.95)
    sparkle(center: CGPoint(x: 650, y: 840), radius: 26, alpha: 0.7)
}

/// Finder volume icon: a refined external drive whose shell carries a soft indigo→plum gradient.
func drawVolumeIcon(_ cg: CGContext) {
    let body = CGRect(x: 120, y: 268, width: 784, height: 488)
    let radius: CGFloat = 132
    let bodyPath = roundedRect(body, radius)

    // Contact shadow + ambient shadow.
    cg.saveGState()
    cg.setShadow(offset: CGSize(width: 0, height: -26), blur: 46, color: rgb(40, 30, 90, 0.34))
    cg.addPath(bodyPath)
    cg.setFillColor(rgb(110, 100, 210))
    cg.fillPath()
    cg.restoreGState()

    // Shell gradient: periwinkle at the top-left, plum at the bottom-right.
    cg.saveGState()
    cg.addPath(bodyPath)
    cg.clip()
    cg.drawLinearGradient(
        linearGradient([rgb(146, 160, 255), rgb(116, 118, 236), rgb(150, 92, 206)], [0, 0.55, 1]),
        start: CGPoint(x: body.minX, y: body.maxY), end: CGPoint(x: body.maxX, y: body.minY), options: [])
    // Soft light bloom ("lumen") from the top-left.
    cg.drawRadialGradient(
        linearGradient([rgb(255, 255, 255, 0.38), rgb(255, 255, 255, 0)], [0, 1]),
        startCenter: CGPoint(x: 300, y: 740), startRadius: 0,
        endCenter: CGPoint(x: 300, y: 740), endRadius: 560, options: [])
    // Front bezel: a slightly deeper band along the bottom edge.
    let band = CGRect(x: body.minX, y: body.minY, width: body.width, height: 150)
    cg.clip(to: band)
    cg.drawLinearGradient(
        linearGradient([rgb(70, 52, 150, 0.30), rgb(60, 40, 130, 0.42)], [0, 1]),
        start: CGPoint(x: 512, y: band.maxY), end: CGPoint(x: 512, y: band.minY), options: [])
    cg.restoreGState()

    // Seam between the top face and the bezel.
    cg.saveGState()
    cg.addPath(bodyPath)
    cg.clip()
    cg.setStrokeColor(rgb(255, 255, 255, 0.28))
    cg.setLineWidth(3)
    cg.move(to: CGPoint(x: body.minX, y: body.minY + 150))
    cg.addLine(to: CGPoint(x: body.maxX, y: body.minY + 150))
    cg.strokePath()
    cg.setStrokeColor(rgb(40, 20, 100, 0.22))
    cg.move(to: CGPoint(x: body.minX, y: body.minY + 147))
    cg.addLine(to: CGPoint(x: body.maxX, y: body.minY + 147))
    cg.strokePath()
    cg.restoreGState()

    // Rim light.
    cg.saveGState()
    cg.addPath(roundedRect(body.insetBy(dx: 2, dy: 2), radius - 2))
    cg.setStrokeColor(rgb(255, 255, 255, 0.42))
    cg.setLineWidth(4)
    cg.strokePath()
    cg.restoreGState()

    // Status LED with a mint glow.
    let led = CGPoint(x: body.maxX - 110, y: body.minY + 75)
    cg.saveGState()
    cg.drawRadialGradient(
        linearGradient([rgb(120, 255, 210, 0.65), rgb(120, 255, 210, 0)], [0, 1]),
        startCenter: led, startRadius: 0, endCenter: led, endRadius: 58, options: [])
    cg.addEllipse(in: CGRect(x: led.x - 17, y: led.y - 17, width: 34, height: 34))
    cg.setFillColor(rgb(80, 232, 180))
    cg.fillPath()
    cg.addEllipse(in: CGRect(x: led.x - 7, y: led.y + 2, width: 11, height: 9))
    cg.setFillColor(rgb(255, 255, 255, 0.8))
    cg.fillPath()
    cg.restoreGState()

    // Grille slots on the bezel.
    cg.saveGState()
    cg.setFillColor(rgb(255, 255, 255, 0.32))
    for i in 0..<5 {
        let x = body.minX + 92 + CGFloat(i) * 40
        cg.addPath(roundedRect(CGRect(x: x, y: body.minY + 56, width: 18, height: 38), 9))
    }
    cg.fillPath()
    cg.restoreGState()

    // Four-point sparkle on the top face.
    let c = CGPoint(x: 512, y: 590), r: CGFloat = 104, w = r * 0.2
    let star = CGMutablePath()
    star.move(to: CGPoint(x: c.x, y: c.y + r))
    star.addQuadCurve(to: CGPoint(x: c.x + r, y: c.y), control: CGPoint(x: c.x + w, y: c.y + w))
    star.addQuadCurve(to: CGPoint(x: c.x, y: c.y - r), control: CGPoint(x: c.x + w, y: c.y - w))
    star.addQuadCurve(to: CGPoint(x: c.x - r, y: c.y), control: CGPoint(x: c.x - w, y: c.y - w))
    star.addQuadCurve(to: CGPoint(x: c.x, y: c.y + r), control: CGPoint(x: c.x - w, y: c.y + w))
    cg.saveGState()
    cg.setShadow(offset: .zero, blur: 30, color: rgb(255, 255, 255, 0.7))
    cg.addPath(star)
    cg.setFillColor(rgb(255, 255, 255, 0.96))
    cg.fillPath()
    cg.restoreGState()
}

func renderPNG(pixels: Int, draw: (CGContext) -> Void = drawIcon) -> Data {
    guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels,
                                     bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                     colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0),
          let context = NSGraphicsContext(bitmapImageRep: rep) else {
        fatalError("Impossibile creare il contesto grafico")
    }
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = context
    let cg = context.cgContext
    cg.setAllowsAntialiasing(true)
    cg.setShouldAntialias(true)
    cg.interpolationQuality = .high
    cg.clear(CGRect(x: 0, y: 0, width: pixels, height: pixels))
    cg.scaleBy(x: CGFloat(pixels) / 1024, y: CGFloat(pixels) / 1024)
    draw(cg)
    context.flushGraphics()
    NSGraphicsContext.restoreGraphicsState()
    guard let data = rep.representation(using: .png, properties: [:]) else { fatalError("PNG non generato") }
    return data
}

let fm = FileManager.default
let variants: [(name: String, pixels: Int)] = [
    ("icon_16x16", 16), ("icon_16x16@2x", 32),
    ("icon_32x32", 32), ("icon_32x32@2x", 64),
    ("icon_128x128", 128), ("icon_128x128@2x", 256),
    ("icon_256x256", 256), ("icon_256x256@2x", 512),
    ("icon_512x512", 512), ("icon_512x512@2x", 1024),
]

/// Renders every iconset size with `draw` and converts it to `<name>.icns` in the output folder.
func writeIcns(named name: String, draw: (CGContext) -> Void) throws {
    let iconset = fm.temporaryDirectory.appendingPathComponent("\(name)-\(ProcessInfo.processInfo.processIdentifier).iconset")
    try? fm.removeItem(at: iconset)
    try fm.createDirectory(at: iconset, withIntermediateDirectories: true)
    for variant in variants {
        try renderPNG(pixels: variant.pixels, draw: draw).write(to: iconset.appendingPathComponent("\(variant.name).png"))
    }
    let icns = outputDir.appendingPathComponent("\(name).icns")
    let task = Process()
    task.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
    task.arguments = ["-c", "icns", iconset.path, "-o", icns.path]
    try task.run()
    task.waitUntilExit()
    try? fm.removeItem(at: iconset)
    guard task.terminationStatus == 0 else {
        FileHandle.standardError.write("iconutil failed for \(name) (\(task.terminationStatus))\n".data(using: .utf8)!)
        exit(1)
    }
    print("Created \(icns.path)")
}

// Optional 1024 px PNG previews.
let env = ProcessInfo.processInfo.environment
if let preview = env["ICON_PREVIEW"] { try renderPNG(pixels: 1024).write(to: URL(fileURLWithPath: preview)) }
if let preview = env["VOLUME_ICON_PREVIEW"] { try renderPNG(pixels: 1024, draw: drawVolumeIcon).write(to: URL(fileURLWithPath: preview)) }

try writeIcns(named: "AppIcon", draw: drawIcon)
try writeIcns(named: "VolumeIcon", draw: drawVolumeIcon)
