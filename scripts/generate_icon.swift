// OatsNotes app icon generator — run from the repo root with: swift scripts/generate_icon.swift
// Draws the icon with CoreGraphics (reproducible, no image assets needed): a macOS-style
// rounded square in a warm oat/wheat gradient, with the standard ~10% transparent margin, and
// a dark charcoal sound-bars glyph (five rounded vertical bars) centred on it.
// Writes assets/icon_1024.png.
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let size: CGFloat = 1024
let cs = CGColorSpace(name: CGColorSpace.sRGB)!
let ctx = CGContext(
    data: nil, width: Int(size), height: Int(size), bitsPerComponent: 8, bytesPerRow: 0,
    space: cs, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(
        srgbRed: CGFloat((hex >> 16) & 0xFF) / 255,
        green: CGFloat((hex >> 8) & 0xFF) / 255,
        blue: CGFloat(hex & 0xFF) / 255, alpha: alpha)
}

func gradient(_ colors: [CGColor]) -> CGGradient {
    CGGradient(colorsSpace: cs, colors: colors as CFArray, locations: nil)!
}

// ── Background: rounded square, 824 pt on the 1024 canvas (Apple's icon grid) ──
let inset: CGFloat = 100
let bg = CGRect(x: inset, y: inset, width: size - 2 * inset, height: size - 2 * inset)
let radius: CGFloat = bg.width * 0.225
let bgPath = CGPath(roundedRect: bg, cornerWidth: radius, cornerHeight: radius, transform: nil)

// soft drop shadow under the tile
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -10), blur: 24, color: color(0x000000, 0.28))
ctx.addPath(bgPath)
ctx.setFillColor(color(0xc9a265))
ctx.fillPath()
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(bgPath)
ctx.clip()
// top (#e8c98a) → bottom (#b98a4a); CoreGraphics' y axis points up
ctx.drawLinearGradient(
    gradient([color(0xe8c98a), color(0xb98a4a)]),
    start: CGPoint(x: bg.midX, y: bg.maxY), end: CGPoint(x: bg.midX, y: bg.minY), options: [])
// a faint highlight along the top edge
ctx.drawLinearGradient(
    gradient([color(0xffffff, 0.18), color(0xffffff, 0)]),
    start: CGPoint(x: bg.midX, y: bg.maxY), end: CGPoint(x: bg.midX, y: bg.maxY - 220),
    options: [])
ctx.restoreGState()

// ── Glyph: five rounded bars, symmetric heights ─────────────────────────────
let heights: [CGFloat] = [0.34, 0.62, 0.86, 0.56, 0.30]
let barW: CGFloat = 70
let gap: CGFloat = 46
let maxH: CGFloat = 470
let totalW = CGFloat(heights.count) * barW + CGFloat(heights.count - 1) * gap
var x = size / 2 - totalW / 2
ctx.setFillColor(color(0x2a2a2a))
for h in heights {
    let bh = maxH * h
    let r = CGRect(x: x, y: size / 2 - bh / 2, width: barW, height: bh)
    ctx.addPath(CGPath(roundedRect: r, cornerWidth: barW / 2, cornerHeight: barW / 2, transform: nil))
    ctx.fillPath()
    x += barW + gap
}

// ── Write PNG ────────────────────────────────────────────────────────────────
let out = URL(fileURLWithPath: "assets/icon_1024.png")
try? FileManager.default.createDirectory(
    at: out.deletingLastPathComponent(), withIntermediateDirectories: true)
let image = ctx.makeImage()!
let dest = CGImageDestinationCreateWithURL(out as CFURL, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(dest, image, nil)
CGImageDestinationFinalize(dest)
print("wrote \(out.path)")
