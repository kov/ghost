// scroll-probe: log what macOS does with a trackpad flick — the finger phase
// and the OS's own momentum (coasting) — as the reference for ghost's Linux glide.
//
// Every scrollWheel event goes to a JSON-lines log; the window shows a summary
// of the last gesture. Build and run on the Mac:
//   swiftc -O main.swift -o scroll-probe && ./scroll-probe [log.jsonl]
// then turn the log into a fixture with to_fixture.py.
import AppKit

let logPath = CommandLine.arguments.count > 1
    ? CommandLine.arguments[1]
    : NSHomeDirectory() + "/scroll-probe.jsonl"
FileManager.default.createFile(atPath: logPath, contents: nil)
let log = FileHandle(forWritingAtPath: logPath)!

func phaseName(_ p: NSEvent.Phase) -> String {
    if p.isEmpty { return "none" }
    var names: [String] = []
    if p.contains(.mayBegin) { names.append("mayBegin") }
    if p.contains(.began) { names.append("began") }
    if p.contains(.stationary) { names.append("stationary") }
    if p.contains(.changed) { names.append("changed") }
    if p.contains(.ended) { names.append("ended") }
    if p.contains(.cancelled) { names.append("cancelled") }
    return names.joined(separator: "|")
}

final class ProbeView: NSView {
    var gesture = 0
    var fingerTravel = 0.0, momentumTravel = 0.0
    var fingerStart = 0.0, fingerEnd = 0.0, momentumStart = 0.0, momentumEnd = 0.0
    var fingerEvents = 0, momentumEvents = 0
    var lines: [String] = ["Flick with two fingers anywhere in this window.",
                           "Log: " + logPath]

    override var acceptsFirstResponder: Bool { true }

    override func scrollWheel(with e: NSEvent) {
        let rec: [String: Any] = [
            "t": e.timestamp,
            "dy": e.scrollingDeltaY,
            "dx": e.scrollingDeltaX,
            "precise": e.hasPreciseScrollingDeltas,
            "phase": phaseName(e.phase),
            "momentum": phaseName(e.momentumPhase),
            "inverted": e.isDirectionInvertedFromDevice,
        ]
        if let data = try? JSONSerialization.data(withJSONObject: rec, options: [.sortedKeys]) {
            log.write(data)
            log.write("\n".data(using: .utf8)!)
        }
        if e.phase.contains(.began) {
            gesture += 1
            fingerTravel = 0; momentumTravel = 0
            fingerEvents = 0; momentumEvents = 0
            fingerStart = e.timestamp; momentumStart = 0; momentumEnd = 0
        }
        if !e.phase.isEmpty {
            fingerTravel += e.scrollingDeltaY; fingerEvents += 1; fingerEnd = e.timestamp
        }
        if !e.momentumPhase.isEmpty {
            if e.momentumPhase.contains(.began) { momentumStart = e.timestamp }
            momentumTravel += e.scrollingDeltaY; momentumEvents += 1; momentumEnd = e.timestamp
        }
        let ms = { (a: Double, b: Double) in Int(((b - a) * 1000).rounded()) }
        lines = [
            "gesture #\(gesture)",
            String(format: "finger:   %.1f pt in %d events over %d ms",
                   fingerTravel, fingerEvents, ms(fingerStart, fingerEnd)),
            momentumStart > 0
                ? String(format: "momentum: %.1f pt in %d events over %d ms (starts %d ms after lift)",
                         momentumTravel, momentumEvents, ms(momentumStart, momentumEnd),
                         ms(fingerEnd, momentumStart))
                : "momentum: none (yet)",
            "Log: " + logPath,
        ]
        needsDisplay = true
    }

    override func draw(_ dirty: NSRect) {
        NSColor.windowBackgroundColor.setFill()
        dirty.fill()
        let attrs: [NSAttributedString.Key: Any] = [
            .font: NSFont.monospacedSystemFont(ofSize: 15, weight: .regular),
            .foregroundColor: NSColor.labelColor,
        ]
        var y = bounds.height - 40
        for l in lines {
            (l as NSString).draw(at: NSPoint(x: 20, y: y), withAttributes: attrs)
            y -= 26
        }
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let win = NSWindow(contentRect: NSRect(x: 200, y: 200, width: 760, height: 520),
                   styleMask: [.titled, .closable, .resizable],
                   backing: .buffered, defer: false)
win.title = "scroll-probe"
let view = ProbeView()
win.contentView = view
win.makeFirstResponder(view)
win.makeKeyAndOrderFront(nil)
app.activate(ignoringOtherApps: true)
app.run()
