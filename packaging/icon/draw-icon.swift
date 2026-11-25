// Draws Hop's 1024 px master icon: a blue rounded square with a white keyboard and an arrow
// toward the PC. Plain shapes only (SF Symbols may not be used in app icons).
// Usage: swift draw-icon.swift out.png
import AppKit

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon-1024.png"
let image = NSImage(size: NSSize(width: size, height: size))
image.lockFocus()

// macOS icon grid: the shape fills 824 of 1024, centred, with a 185 px corner radius.
let inset: CGFloat = 100
let rect = NSRect(x: inset, y: inset, width: size - 2 * inset, height: size - 2 * inset)
let shape = NSBezierPath(roundedRect: rect, xRadius: 185, yRadius: 185)
NSGraphicsContext.current?.saveGraphicsState()
let shadow = NSShadow()
shadow.shadowColor = NSColor.black.withAlphaComponent(0.28)
shadow.shadowBlurRadius = 24
shadow.shadowOffset = NSSize(width: 0, height: -10)
shadow.set()
NSColor(srgbRed: 0.10, green: 0.36, blue: 0.95, alpha: 1).setFill()
shape.fill()
NSGraphicsContext.current?.restoreGraphicsState()
let top = NSColor(srgbRed: 0.29, green: 0.55, blue: 1.0, alpha: 1)
let bottom = NSColor(srgbRed: 0.07, green: 0.29, blue: 0.86, alpha: 1)
NSGradient(colors: [top, bottom])!.draw(in: shape, angle: -90)

// The keyboard: a white body with keys cut out in the background blue.
let body = NSRect(x: 232, y: 420, width: 560, height: 330)
NSColor.white.setFill()
NSBezierPath(roundedRect: body, xRadius: 64, yRadius: 64).fill()
let keyBlue = NSColor(srgbRed: 0.13, green: 0.40, blue: 0.95, alpha: 1)
keyBlue.setFill()
let (pad, gap, keyH): (CGFloat, CGFloat, CGFloat) = (46, 20, 50)
let rowW = body.width - 2 * pad
func row(_ y: CGFloat, keys: Int, indent: CGFloat = 0) {
    let w = (rowW - 2 * indent - CGFloat(keys - 1) * gap) / CGFloat(keys)
    for i in 0..<keys {
        let x = body.minX + pad + indent + CGFloat(i) * (w + gap)
        NSBezierPath(roundedRect: NSRect(x: x, y: y, width: w, height: keyH), xRadius: 14, yRadius: 14).fill()
    }
}
let firstRow = body.maxY - pad - keyH
row(firstRow, keys: 7)
row(firstRow - (keyH + gap), keys: 6, indent: 24)
// The space bar row: a short key, the bar, a short key.
let y3 = firstRow - 2 * (keyH + gap)
let side: CGFloat = 78
NSBezierPath(roundedRect: NSRect(x: body.minX + pad, y: y3, width: side, height: keyH), xRadius: 14, yRadius: 14).fill()
NSBezierPath(roundedRect: NSRect(x: body.minX + pad + side + gap, y: y3, width: rowW - 2 * (side + gap), height: keyH), xRadius: 14, yRadius: 14).fill()
NSBezierPath(roundedRect: NSRect(x: body.maxX - pad - side, y: y3, width: side, height: keyH), xRadius: 14, yRadius: 14).fill()

// The arrow toward the PC, under the keyboard.
NSColor.white.setFill()
let arrow = NSBezierPath()
let (ay, shaft, headW, headL): (CGFloat, CGFloat, CGFloat, CGFloat) = (285, 44, 150, 110)
let (ax0, ax1): (CGFloat, CGFloat) = (352, 672)
arrow.move(to: NSPoint(x: ax0, y: ay - shaft / 2))
arrow.line(to: NSPoint(x: ax1 - headL, y: ay - shaft / 2))
arrow.line(to: NSPoint(x: ax1 - headL, y: ay - headW / 2))
arrow.line(to: NSPoint(x: ax1, y: ay))
arrow.line(to: NSPoint(x: ax1 - headL, y: ay + headW / 2))
arrow.line(to: NSPoint(x: ax1 - headL, y: ay + shaft / 2))
arrow.line(to: NSPoint(x: ax0, y: ay + shaft / 2))
arrow.close()
arrow.lineJoinStyle = .round
arrow.fill()
NSBezierPath(ovalIn: NSRect(x: ax0 - shaft / 2, y: ay - shaft / 2, width: shaft, height: shaft)).fill()

image.unlockFocus()
let rep = NSBitmapImageRep(data: image.tiffRepresentation!)!
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
