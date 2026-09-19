#!/usr/bin/env swift
// Generates AppIcon.icns from a simple, code-drawn design (a rounded-square background + the
// server.rack SF Symbol, matching the same glyph already used in MenuBarViews.swift) — no external
// image tools/assets needed, just AppKit. Run via scripts/generate-icon.sh, which wraps this with the
// iconset/sips/iconutil steps.
//
// Usage: swift make-icon.swift <output-1024.png>
import AppKit

let arguments = CommandLine.arguments
guard arguments.count == 2 else {
    FileHandle.standardError.write(Data("usage: make-icon.swift <output.png>\n".utf8))
    exit(1)
}
let outputPath = arguments[1]
let size = 1024.0

let image = NSImage(size: NSSize(width: size, height: size))
image.lockFocus()

// Background: a rounded square (macOS's own icon masking will still apply its own corner curve on
// top of this at render time, but drawing our own keeps the flat-PNG preview/Finder-icon-before-mask
// looking intentional rather than a plain square).
let cornerRadius = size * 0.2237 // matches Apple's standard large-icon corner ratio
let backgroundRect = NSRect(x: 0, y: 0, width: size, height: size)
let backgroundPath = NSBezierPath(roundedRect: backgroundRect, xRadius: cornerRadius, yRadius: cornerRadius)
let gradient = NSGradient(colors: [
    NSColor(calibratedRed: 0.22, green: 0.47, blue: 0.98, alpha: 1),
    NSColor(calibratedRed: 0.36, green: 0.24, blue: 0.86, alpha: 1),
])
gradient?.draw(in: backgroundPath, angle: -60)

// Foreground glyph: server.rack, matching the menu bar's own icon for visual continuity. A palette
// color configuration pre-tints the symbol white directly — no template/blend-mode masking needed.
let symbolConfig = NSImage.SymbolConfiguration(pointSize: size * 0.5, weight: .semibold)
    .applying(NSImage.SymbolConfiguration(paletteColors: [.white]))
if let symbol = NSImage(systemSymbolName: "server.rack", accessibilityDescription: nil)?.withSymbolConfiguration(symbolConfig) {
    let symbolSize = symbol.size
    let origin = NSPoint(x: (size - symbolSize.width) / 2, y: (size - symbolSize.height) / 2)
    symbol.draw(at: origin, from: .zero, operation: .sourceOver, fraction: 1.0)
} else {
    FileHandle.standardError.write(Data("warning: could not load server.rack symbol\n".utf8))
}

image.unlockFocus()

guard let tiff = image.tiffRepresentation, let bitmap = NSBitmapImageRep(data: tiff), let png = bitmap.representation(using: .png, properties: [:]) else {
    FileHandle.standardError.write(Data("failed to render PNG\n".utf8))
    exit(1)
}
try png.write(to: URL(fileURLWithPath: outputPath))
