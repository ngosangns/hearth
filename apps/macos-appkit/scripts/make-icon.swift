#!/usr/bin/env swift
// Render AppIcon.iconset PNGs: a dark ember squircle with an amber flame.fill
// SF Symbol. bundle.sh then runs `iconutil -c icns` on the iconset.
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
    let rect = NSRect(x: 0, y: 0, width: p, height: p)

    // Squircle-ish rounded rect, ember gradient (deep charcoal → warm brown).
    let bg = NSBezierPath(roundedRect: rect, xRadius: p * 0.23, yRadius: p * 0.23)
    let gradient = NSGradient(colors: [
        NSColor(calibratedRed: 0.32, green: 0.16, blue: 0.08, alpha: 1),
        NSColor(calibratedRed: 0.13, green: 0.07, blue: 0.05, alpha: 1),
    ])!
    gradient.draw(in: bg, angle: -90)

    // Amber flame, centered, ~62% of the tile.
    let palette = NSImage.SymbolConfiguration(paletteColors: [
        NSColor(calibratedRed: 1.0, green: 0.62, blue: 0.24, alpha: 1),
    ])
    let config = NSImage.SymbolConfiguration(pointSize: p * 0.62, weight: .regular)
        .applying(palette)
    if let flame = NSImage(systemSymbolName: "flame.fill", accessibilityDescription: nil)?
        .withSymbolConfiguration(config) {
        let s = flame.size
        flame.draw(in: NSRect(x: (p - s.width) / 2, y: (p - s.height) / 2,
                              width: s.width, height: s.height))
    }

    return rep.representation(using: .png, properties: [:])
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
