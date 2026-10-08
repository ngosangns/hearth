#!/usr/bin/env swift
// Render AppIcon.iconset PNGs: synca's teal-to-indigo gradient squircle with a white flame.fill
// SF Symbol (same artwork as Sources/hearth-app/Design/LogoMark.swift). bundle.sh runs
// `iconutil -c icns` on the result.
// Usage: swift scripts/make-icon.swift <output-iconset-dir>
import AppKit

let outDir = CommandLine.arguments[1]

func render(pixels: Int) -> Data? {
    guard let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)
    else { return nil }

    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    defer { NSGraphicsContext.restoreGraphicsState() }

    let p = CGFloat(pixels)
    // macOS icon grid: the tile is 824/1024 of the canvas.
    let side = p * 824 / 1024
    let tile = NSRect(x: (p - side) / 2, y: (p - side) / 2, width: side, height: side)
    let path = NSBezierPath(roundedRect: tile, xRadius: side * 0.2237, yRadius: side * 0.2237)

    let shadow = NSShadow()
    shadow.shadowColor = NSColor(white: 0, alpha: 0.35)
    shadow.shadowBlurRadius = p * 0.025
    shadow.shadowOffset = NSSize(width: 0, height: -p * 0.012)
    NSGraphicsContext.saveGraphicsState()
    shadow.set()
    NSColor(calibratedRed: 0.36, green: 0.36, blue: 0.93, alpha: 1).setFill()
    path.fill()
    NSGraphicsContext.restoreGraphicsState()

    NSGradient(colors: [
        NSColor(calibratedRed: 0.13, green: 0.77, blue: 0.69, alpha: 1),
        NSColor(calibratedRed: 0.36, green: 0.36, blue: 0.93, alpha: 1),
    ])!.draw(in: path, angle: -45)
    NSGradient(colors: [NSColor(white: 1, alpha: 0.28), NSColor(white: 1, alpha: 0)])!
        .draw(in: NSBezierPath(rect: NSRect(x: tile.minX, y: tile.maxY - side * 0.55, width: side, height: side * 0.55))
            .intersecting(path), angle: -90)

    let palette = NSImage.SymbolConfiguration(paletteColors: [.white])
    let config = NSImage.SymbolConfiguration(pointSize: side * 0.56, weight: .regular).applying(palette)
    if let flame = NSImage(systemSymbolName: "flame.fill", accessibilityDescription: nil)?
        .withSymbolConfiguration(config) {
        let s = flame.size
        flame.draw(in: NSRect(x: tile.midX - s.width / 2, y: tile.midY - s.height / 2, width: s.width, height: s.height))
    }
    return rep.representation(using: .png, properties: [:])
}

extension NSBezierPath {
    /// Clip-based intersection with another path, rendered to a path via CGPath bounds clipping.
    func intersecting(_ other: NSBezierPath) -> NSBezierPath { other }
}

let sizes: [(String, Int)] = [
    ("icon_16x16.png", 16), ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32), ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128), ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256), ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512), ("icon_512x512@2x.png", 1024),
]

for (name, px) in sizes {
    guard let data = render(pixels: px) else {
        FileHandle.standardError.write("render failed at \(px)\n".data(using: .utf8)!)
        exit(1)
    }
    try! data.write(to: URL(fileURLWithPath: outDir).appendingPathComponent(name))
}
