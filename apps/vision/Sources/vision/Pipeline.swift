// Pipeline.swift — PIPELINE module (filled by the pipeline agent).
//
// Responsibility: orchestrate capture -> inference -> events. It owns the run
// loop: pull Frames from a FrameSource, run the Detector, derive motion
// (frame-to-frame change vs the sensitivity threshold), batch/throttle results,
// and hand VisionEvents to a sink (the ipc module). It also owns mutable run
// state driven by Ops: current source, sensitivity, watch lifecycle.
//
// Defensive: motion is derived from frame deltas only; nothing here records or
// transmits pixels. Detections carry boxes/labels/counts, never identity. The
// luma sampling below reads a frame into a small fixed grid of brightness
// values purely to measure CHANGE between consecutive frames; that grid never
// leaves this process and is discarded each tick.
//
// DESIGN — pure + headlessly testable. The hard logic (motion threshold,
// presence enter/exit dwell with anti-flicker hysteresis, burst debounce, alert
// hysteresis) lives in small value types with no I/O, mirroring the HUD's
// listening-hysteresis discipline (separate ENTER/EXIT thresholds with a band
// between them that holds state, plus consecutive-frame dwell before a
// transition). The actor wires a FrameSource + Detector through those types and
// emits VisionEvents; every step is exercisable with synthesized luma grids and
// synthesized detections — no camera, no screen, no TCC, no socket.

import Foundation
import CoreGraphics
import CoreVideo

// ===========================================================================
// Public seam — kept verbatim so ipc/main compile against the same surface.
// ===========================================================================

/// Where the pipeline sends VisionEvents (implemented by the ipc module).
public protocol EventSink: Sendable {
    func emit(_ event: VisionEvent) async
}

/// Tunables that change with `set.sensitivity` and source selection.
public struct PipelineConfig: Sendable, Equatable {
    /// Detection/motion sensitivity in 0...1; higher = more sensitive
    /// (lower confidence floor, smaller motion magnitude triggers an event).
    public var sensitivity: Double
    /// Which built-in detectors to run.
    public var detectors: DetectorSet
    /// Max detection events per second (throttle for high-fps live capture).
    public var maxEventsPerSecond: Double

    public init(sensitivity: Double = 0.5,
                detectors: DetectorSet = .liveDefault,
                maxEventsPerSecond: Double = 10) {
        self.sensitivity = sensitivity
        self.detectors = detectors
        self.maxEventsPerSecond = maxEventsPerSecond
    }

    /// Map sensitivity -> a confidence floor for the detector (0...1).
    /// sensitivity 0 -> floor 0.9 (very strict); 1 -> floor 0.1 (very loose).
    public var minConfidence: Double {
        let s = max(0, min(1, sensitivity))
        return 0.9 - 0.8 * s
    }

    /// Map sensitivity -> a MOTION magnitude floor (0...1). A frame-to-frame
    /// luma change must exceed this to count as motion. sensitivity 0 -> 0.40
    /// (only big changes), 1 -> 0.02 (almost any change). Inverse of sensitivity
    /// so "more sensitive" means "smaller motion triggers".
    public var motionThreshold: Double {
        let s = max(0, min(1, sensitivity))
        return 0.40 - 0.38 * s
    }
}

// ===========================================================================
// Motion detection — frame-to-frame luma delta.
// ===========================================================================

/// A small fixed-size brightness grid sampled from a frame, used ONLY to
/// measure change between consecutive frames. Values are luma in 0...1. This is
/// a derived statistic, never the pixels themselves; it is computed, compared,
/// and discarded each tick. Deliberately Equatable for deterministic tests.
public struct LumaGrid: Sendable, Equatable {
    /// Cells per side (the grid is `side x side`).
    public let side: Int
    /// Row-major brightness values, length == side*side, each clamped to 0...1.
    public let cells: [Double]

    public init(side: Int, cells: [Double]) {
        precondition(side > 0, "LumaGrid side must be positive")
        precondition(cells.count == side * side, "LumaGrid cells must be side*side")
        self.side = side
        self.cells = cells.map { max(0, min(1, $0)) }
    }

    /// A uniform grid (all cells the same brightness) — handy for tests + as a
    /// safe fallback when a frame cannot be sampled.
    public static func uniform(side: Int, value: Double) -> LumaGrid {
        LumaGrid(side: side, cells: Array(repeating: value, count: side * side))
    }

    /// Sample a frame's CGImage into a `side x side` luma grid. Returns nil if
    /// the frame carries no CGImage (live CVPixelBuffer-only frames are sampled
    /// by the capture/inference seam in the real build; for the headlessly
    /// testable path we sample CGImages). Pure read; pixels never leave here.
    public static func sample(_ frame: Frame, side: Int = 12) -> LumaGrid? {
        guard side > 0, let cg = frame.cgImage else { return nil }
        return sample(cgImage: cg, side: side)
    }

    /// Render a CGImage down into a `side x side` grayscale buffer and read the
    /// per-cell brightness. Uses a tiny grayscale context so this is cheap and
    /// allocation-bounded regardless of source resolution.
    public static func sample(cgImage cg: CGImage, side: Int) -> LumaGrid? {
        guard side > 0 else { return nil }
        let gray = CGColorSpaceCreateDeviceGray()
        guard let ctx = CGContext(
            data: nil, width: side, height: side, bitsPerComponent: 8,
            bytesPerRow: side, space: gray,
            bitmapInfo: CGImageAlphaInfo.none.rawValue)
        else { return nil }
        ctx.interpolationQuality = .low
        ctx.draw(cg, in: CGRect(x: 0, y: 0, width: side, height: side))
        guard let buf = ctx.data else { return nil }
        let bytes = buf.bindMemory(to: UInt8.self, capacity: side * side)
        var cells = [Double](repeating: 0, count: side * side)
        for i in 0..<(side * side) {
            cells[i] = Double(bytes[i]) / 255.0
        }
        return LumaGrid(side: side, cells: cells)
    }
}

/// Result of comparing two consecutive luma grids.
public struct MotionResult: Sendable, Equatable {
    /// Overall change magnitude in 0...1 (mean absolute per-cell luma delta).
    public let magnitude: Double
    /// The normalized bounding box (Vision coords, origin bottom-left) covering
    /// the cells that changed most — a coarse "where" for the motion.
    public let region: DetectionBox
    /// Whether `magnitude` crossed the supplied threshold.
    public let exceeded: Bool

    public init(magnitude: Double, region: DetectionBox, exceeded: Bool) {
        self.magnitude = magnitude
        self.region = region
        self.exceeded = exceeded
    }

    /// "No motion" result over the whole frame.
    public static let none = MotionResult(magnitude: 0, region: .full, exceeded: false)
}

/// Pure frame-diff motion detector. Holds the previous grid; `step` compares the
/// next grid and reports magnitude + region + whether it crossed `threshold`.
/// No I/O, fully deterministic — the unit tests drive it with literal grids.
public struct MotionDetector: Sendable {
    /// Cells whose absolute delta is at least this fraction of the max delta are
    /// counted toward the active region (so the region tracks the strongest
    /// change, not faint global noise).
    public static let regionDeltaFraction = 0.5

    private var previous: LumaGrid?

    public init() {}

    /// Reset to a clean slate (call on watch start / source change so the first
    /// frame of a new run is a baseline, never a spurious huge delta).
    public mutating func reset() { previous = nil }

    /// Compare the next grid against the previous one.
    /// - The FIRST grid after a reset establishes a baseline and reports
    ///   `.none` (no previous frame to diff against — never a false motion hit).
    /// - Grids of mismatched `side` re-baseline (treated like the first grid).
    public mutating func step(_ grid: LumaGrid, threshold: Double) -> MotionResult {
        defer { previous = grid }
        guard let prev = previous, prev.side == grid.side else {
            return .none
        }
        let n = grid.cells.count
        guard n > 0 else { return .none }

        var sumAbs = 0.0
        var maxAbs = 0.0
        var deltas = [Double](repeating: 0, count: n)
        for i in 0..<n {
            let d = abs(grid.cells[i] - prev.cells[i])
            deltas[i] = d
            sumAbs += d
            if d > maxAbs { maxAbs = d }
        }
        let magnitude = sumAbs / Double(n)
        let exceeded = magnitude > threshold
        let region = Self.regionForDeltas(deltas, side: grid.side, maxAbs: maxAbs)
        return MotionResult(magnitude: magnitude, region: region, exceeded: exceeded)
    }

    /// Bounding box (normalized, origin bottom-left) over the cells whose delta
    /// is a meaningful fraction of the max delta. If nothing changed, the whole
    /// frame. Image rows are top-down; Vision y is bottom-up, so we flip y.
    static func regionForDeltas(_ deltas: [Double], side: Int, maxAbs: Double) -> DetectionBox {
        guard maxAbs > 0 else { return .full }
        let floor = maxAbs * regionDeltaFraction
        var minCol = side, maxCol = -1, minRow = side, maxRow = -1
        for r in 0..<side {
            for c in 0..<side where deltas[r * side + c] >= floor {
                if c < minCol { minCol = c }
                if c > maxCol { maxCol = c }
                if r < minRow { minRow = r }
                if r > maxRow { maxRow = r }
            }
        }
        guard maxCol >= 0 else { return .full }
        let s = Double(side)
        let x = Double(minCol) / s
        let w = Double(maxCol - minCol + 1) / s
        // Flip rows: top image row (0) maps to the TOP of the frame, which in
        // Vision's bottom-left origin is the HIGH y. So y = 1 - (bottomRow+1)/s.
        let yTopExclusive = Double(maxRow + 1) / s
        let y = 1.0 - yTopExclusive
        let h = Double(maxRow - minRow + 1) / s
        return DetectionBox(x: x, y: y, width: w, height: h)
    }
}

// ===========================================================================
// Presence state machine — absent <-> present with dwell + hysteresis.
// ===========================================================================

/// Whether a subject (human/animal/motion, per the watcher's interest) is
/// currently considered present. Anti-flicker: a transition needs sustained
/// evidence, and the enter/exit evidence thresholds differ (a band that holds
/// the current state), exactly like the HUD's idle<->listening discipline.
public enum Presence: String, Sendable, Equatable {
    case absent
    case present
}

/// Configuration for the presence machine. Defaults mirror the HUD's pairing:
/// enter requires a higher bar than clears exit (a hysteresis band), and a
/// transition requires several consecutive in-band frames (dwell).
public struct PresenceConfig: Sendable, Equatable {
    /// To go absent -> present, this many CONSECUTIVE frames must show evidence
    /// at or above `enterThreshold`.
    public var enterDwell: Int
    /// To go present -> absent, this many CONSECUTIVE frames must show evidence
    /// strictly below `exitThreshold`.
    public var exitDwell: Int
    /// Evidence (e.g. max detection confidence, or motion magnitude) at/above
    /// this promotes toward present. MUST be >= exitThreshold (hysteresis band).
    public var enterThreshold: Double
    /// Evidence strictly below this counts toward exit. The gap to
    /// `enterThreshold` is the anti-flicker band.
    public var exitThreshold: Double

    public init(enterDwell: Int = 2, exitDwell: Int = 6,
                enterThreshold: Double = 0.5, exitThreshold: Double = 0.35) {
        precondition(enterDwell >= 1 && exitDwell >= 1, "dwell must be >= 1")
        precondition(enterThreshold >= exitThreshold, "enter must be >= exit (hysteresis band)")
        self.enterDwell = enterDwell
        self.exitDwell = exitDwell
        self.enterThreshold = enterThreshold
        self.exitThreshold = exitThreshold
    }
}

/// Pure presence state machine. Feed it per-frame `evidence` in 0...1; it
/// returns the (possibly unchanged) state and whether THIS frame caused a
/// transition. No timers, no I/O — dwell is counted in frames so tests are
/// fully deterministic. Evidence in the band [exitThreshold, enterThreshold)
/// while neither dwell is satisfied holds the current state (no thrash).
public struct PresenceMachine: Sendable, Equatable {
    public let config: PresenceConfig
    public private(set) var state: Presence
    /// Consecutive frames at/above enterThreshold while absent.
    private var enterStreak: Int = 0
    /// Consecutive frames below exitThreshold while present.
    private var exitStreak: Int = 0

    public init(config: PresenceConfig = PresenceConfig(), initial: Presence = .absent) {
        self.config = config
        self.state = initial
    }

    /// One frame of evidence. Returns true iff the state changed this frame.
    @discardableResult
    public mutating func update(evidence: Double) -> Bool {
        switch state {
        case .absent:
            if evidence >= config.enterThreshold {
                enterStreak += 1
                if enterStreak >= config.enterDwell {
                    state = .present
                    enterStreak = 0
                    exitStreak = 0
                    return true
                }
            } else {
                // Any frame that fails the enter bar breaks the run. Frames in
                // the hysteresis band do not advance the enter streak.
                enterStreak = 0
            }
            return false

        case .present:
            if evidence < config.exitThreshold {
                exitStreak += 1
                if exitStreak >= config.exitDwell {
                    state = .absent
                    enterStreak = 0
                    exitStreak = 0
                    return true
                }
            } else {
                // Evidence at/above exitThreshold (incl. the hysteresis band)
                // resets the exit run — presence is "sticky".
                exitStreak = 0
            }
            return false
        }
    }
}

// ===========================================================================
// Event debounce / burst aggregation.
// ===========================================================================

/// Decision returned by the debouncer for one candidate emission.
public enum DebounceDecision: Sendable, Equatable {
    /// Emit now (the carried timestamp becomes the new "last emitted").
    case emit
    /// Suppress (too soon since the last emission — collapsed into the burst).
    case suppress
}

/// Rate-limit / burst-collapse for a single event channel. A burst of frames in
/// quick succession collapses to one emission per `minInterval`; the FIRST of a
/// burst always emits (lead-edge), the rest within the window are suppressed.
/// Time is supplied by the caller (frame timestamps), so it is deterministic and
/// testable with literal times — no wall clock.
public struct Debouncer: Sendable, Equatable {
    /// Minimum seconds between emissions on this channel.
    public let minInterval: TimeInterval
    private var lastEmit: TimeInterval?

    public init(minInterval: TimeInterval) {
        precondition(minInterval >= 0, "minInterval must be >= 0")
        self.minInterval = minInterval
    }

    public init(maxEventsPerSecond: Double) {
        self.init(minInterval: maxEventsPerSecond > 0 ? 1.0 / maxEventsPerSecond : 0)
    }

    /// Reset so the next candidate emits immediately (call on run start).
    public mutating func reset() { lastEmit = nil }

    /// Decide whether a candidate at `time` may emit. Lead-edge: first candidate
    /// (or first after the window elapsed) emits; subsequent ones within the
    /// window are suppressed. Out-of-order/equal timestamps suppress (never
    /// emit "in the past").
    public mutating func admit(at time: TimeInterval) -> DebounceDecision {
        guard let last = lastEmit else {
            lastEmit = time
            return .emit
        }
        if time - last >= minInterval {
            lastEmit = time
            return .emit
        }
        return .suppress
    }
}

/// Collapse a burst of detections into one aggregated event. Merges several
/// frames' detections, deduplicating by (kind, label) keeping the highest
/// confidence, so a flickering detector that reports the same object across N
/// frames yields ONE detection in the emitted event, not N.
public enum BurstAggregator {
    /// Merge detections from a burst. Keyed by (kind,label); for each key the
    /// highest-confidence box wins. Output order is stable: sorted by kind
    /// (declaration order) then label then descending confidence, so the
    /// emitted event is deterministic.
    public static func merge(_ bursts: [[Detection]]) -> [Detection] {
        var best: [String: Detection] = [:]
        for frame in bursts {
            for d in frame {
                let key = d.kind.rawValue + "\u{1f}" + d.label
                if let existing = best[key], existing.confidence >= d.confidence {
                    continue
                }
                best[key] = d
            }
        }
        let kindOrder = Detection.Kind.allCases.enumerated()
            .reduce(into: [String: Int]()) { $0[$1.element.rawValue] = $1.offset }
        return best.values.sorted { a, b in
            let ka = kindOrder[a.kind.rawValue] ?? 0
            let kb = kindOrder[b.kind.rawValue] ?? 0
            if ka != kb { return ka < kb }
            if a.label != b.label { return a.label < b.label }
            return a.confidence > b.confidence
        }
    }
}

// ===========================================================================
// Alert hysteresis — a sticky boolean alarm with raise/clear dwell + band.
// ===========================================================================

/// Configuration for the alert latch. Same hysteresis shape as presence, framed
/// as "raise" (cross up) / "clear" (cross down) with a holding band.
public struct AlertConfig: Sendable, Equatable {
    public var raiseDwell: Int
    public var clearDwell: Int
    public var raiseThreshold: Double
    public var clearThreshold: Double

    public init(raiseDwell: Int = 2, clearDwell: Int = 4,
                raiseThreshold: Double = 0.6, clearThreshold: Double = 0.4) {
        precondition(raiseDwell >= 1 && clearDwell >= 1, "dwell must be >= 1")
        precondition(raiseThreshold >= clearThreshold, "raise must be >= clear (hysteresis band)")
        self.raiseDwell = raiseDwell
        self.clearDwell = clearDwell
        self.raiseThreshold = raiseThreshold
        self.clearThreshold = clearThreshold
    }
}

/// Pure alert latch. `raised` flips true only after `raiseDwell` consecutive
/// frames at/above `raiseThreshold`, and back to false only after `clearDwell`
/// consecutive frames below `clearThreshold`. The band between holds the latch.
public struct AlertHysteresis: Sendable, Equatable {
    public let config: AlertConfig
    public private(set) var raised: Bool
    private var raiseStreak = 0
    private var clearStreak = 0

    public init(config: AlertConfig = AlertConfig(), initiallyRaised: Bool = false) {
        self.config = config
        self.raised = initiallyRaised
    }

    /// One frame of `level` in 0...1. Returns true iff `raised` changed.
    @discardableResult
    public mutating func update(level: Double) -> Bool {
        if !raised {
            if level >= config.raiseThreshold {
                raiseStreak += 1
                if raiseStreak >= config.raiseDwell {
                    raised = true; raiseStreak = 0; clearStreak = 0
                    return true
                }
            } else {
                raiseStreak = 0
            }
            return false
        } else {
            if level < config.clearThreshold {
                clearStreak += 1
                if clearStreak >= config.clearDwell {
                    raised = false; raiseStreak = 0; clearStreak = 0
                    return true
                }
            } else {
                clearStreak = 0
            }
            return false
        }
    }
}

// ===========================================================================
// Pipeline actor — wires the pure pieces to a FrameSource + Detector + sink.
// ===========================================================================

/// Orchestrates one app's vision work. An actor so Op handling + the run loop
/// share mutable state safely. The Op-driven lifecycle and the per-frame
/// processing both route through the pure value types above, so the wire
/// behavior is fully reproducible in tests via `processFrame`.
public actor Pipeline {
    private let detector: Detector
    /// The Sound Analysis classifier for the classify.sound op (the AUDIO analog
    /// of `detector`). Defaults to the real built-in `SoundEngine`. Injectable so
    /// tests can drive a stub; the production wiring uses `SoundEngine`. The
    /// classify.sound op classifies ONE supplied clip — it never opens the mic.
    private let soundClassifier: SoundClassifier
    private let sink: EventSink
    private var config: PipelineConfig

    // Run state.
    private var currentSource: CaptureSource?
    private var state: VisionEvent.WatchState = .idle
    private var frameIndex: UInt64 = 0

    // Pure per-frame machinery.
    private var motion = MotionDetector()
    private var presence: PresenceMachine
    private var detectionDebounce: Debouncer
    private var motionDebounce: Debouncer
    private var perfDebounce: Debouncer

    // Perf accounting. `inferenceMsSamples` collects the REAL measured per-frame
    // inference latencies (ms) so an emitted vision.perf carries genuine p50/p95
    // over the frames seen this run — never a placeholder. Reset per run.
    private var inferenceMsSamples: [Double] = []
    /// The compute-unit tag reported on vision.perf (e.g. "all" = ANE/GPU).
    private let computeUnitTag: String

    // The currently-running watch/analyze task (cancelled on stop / new run).
    private var runTask: Task<Void, Never>?

    // CONTINUOUS SCREEN CONTEXT (#42): the running continuous screen-context loop
    // task, distinct from `runTask` so the OFF-default continuous OCR loop has its
    // OWN lifecycle (a `screen.context.start` op begins it; `screen.context.stop`
    // or the lifecycle `stop` cancels it cleanly — which makes the loop emit its
    // honest watching=false exit). nil whenever no continuous loop is active (the
    // shipped default). Device-gated at capture exactly like every other source.
    private var contextTask: Task<Void, Never>?

    public init(detector: Detector, sink: EventSink, config: PipelineConfig = PipelineConfig(),
                computeUnitTag: String = VisionEngine.computeUnitTag,
                soundClassifier: SoundClassifier = SoundEngine()) {
        self.detector = detector
        self.soundClassifier = soundClassifier
        self.sink = sink
        self.config = config
        self.computeUnitTag = computeUnitTag
        self.presence = PresenceMachine()
        self.detectionDebounce = Debouncer(maxEventsPerSecond: config.maxEventsPerSecond)
        self.motionDebounce = Debouncer(maxEventsPerSecond: config.maxEventsPerSecond)
        self.perfDebounce = Debouncer(maxEventsPerSecond: config.maxEventsPerSecond)
    }

    // -----------------------------------------------------------------------
    // Op handling.
    // -----------------------------------------------------------------------

    /// Apply one Op. Lifecycle ops (start/stop/watch/analyze) reset the per-run
    /// machinery and (for watch/analyze) start a frame-driving task off the
    /// injected FrameSource factory. `set.sensitivity` retunes live.
    public func handle(_ op: Op) async {
        switch op {
        case .start, .refresh, .status:
            await emitStatus(message: nil)

        case .stop, .watchStop:
            // A lifecycle stop also tears down the continuous screen-context loop
            // (so a daemon "stop" honestly ends the WATCHING state); watch.stop is
            // the live-watch stop and likewise stops the context loop.
            stopScreenContextLoop()
            await stopRun(to: .stopped)

        case let .watchStart(source):
            await beginRun(source: source, state: .watching)

        case let .analyzeFile(path):
            await beginRun(source: .file(path: path), state: .analyzing)

        case let .setSensitivity(value):
            config.sensitivity = max(0, min(1, value))
            await emitStatus(message: nil)

        case let .readScreen(source):
            await readScreenOnce(source: source, query: nil)

        case let .describeCapture(path, source):
            await describeCaptureOnce(source: source, path: path)

        case let .classifySound(path):
            await classifySoundOnce(path: path)

        case let .readHandwriting(source):
            await readHandwritingOnce(source: source)

        case let .scanDocument(source):
            await scanDocumentOnce(source: source)

        case let .screenContextStart(source, intervalSecs):
            // CONTINUOUS SCREEN CONTEXT (#42): start the device-gated periodic OCR
            // loop on its OWN task. OFF by default — the daemon only sends this op
            // when [screen_context].enabled. Idempotent: a fresh start cancels any
            // prior loop first.
            startScreenContextLoop(source: source, intervalSeconds: intervalSecs)

        case .screenContextStop:
            stopScreenContextLoop()

        case let .unknown(raw):
            await sink.emit(.error(code: "bad_op", message: "unrecognized op line", source: nil))
            _ = raw
        }
    }

    /// The FrameSource factory the run loop uses to open a source. DEFAULTS to
    /// the ZERO-FRAME `StubFrameSource` so a Pipeline that is never wired produces
    /// no frames (and never opens a device). The PRODUCTION socket path MUST
    /// inject the real factory (`CaptureSourceFactory.make`) via
    /// `useFrameSourceFactory(_:)`; tests inject their own stub/denied sources.
    /// The pipeline NEVER opens a device itself — capture is entirely behind this
    /// seam.
    public var frameSourceFactory: @Sendable (CaptureSource) -> FrameSource = { StubFrameSource(source: $0) }

    /// Inject the FrameSource factory on this actor (the actor-isolated `var`
    /// can only be set from inside the actor's isolation). The production
    /// socket-served path calls this with `CaptureSourceFactory.make` so
    /// analyze.file / watch.start reach the REAL FileSource/Camera/Screen rather
    /// than the zero-frame stub.
    public func useFrameSourceFactory(_ factory: @escaping @Sendable (CaptureSource) -> FrameSource) {
        self.frameSourceFactory = factory
    }

    private func resetRunState() {
        frameIndex = 0
        motion.reset()
        presence = PresenceMachine()
        detectionDebounce = Debouncer(maxEventsPerSecond: config.maxEventsPerSecond)
        motionDebounce = Debouncer(maxEventsPerSecond: config.maxEventsPerSecond)
        perfDebounce = Debouncer(maxEventsPerSecond: config.maxEventsPerSecond)
        inferenceMsSamples.removeAll(keepingCapacity: true)
    }

    private func beginRun(source: CaptureSource, state newState: VisionEvent.WatchState) async {
        runTask?.cancel()
        runTask = nil
        resetRunState()
        currentSource = source
        state = newState

        let fs = frameSourceFactory(source)
        let auth = await fs.authorization()
        switch auth {
        case .denied, .restricted:
            await sink.emit(.error(code: "tcc_denied",
                                   message: "capture not authorized for \(source.tag)",
                                   source: source.tag))
            await emitAuthStatus(source: source, auth: auth)
            state = .stopped
            currentSource = nil
            return
        case .authorized, .notApplicable, .notDetermined:
            break
        }
        await emitAuthStatus(source: source, auth: auth)

        // Drive frames off the source on a child task; each frame routes through
        // the pure processor (deterministic events), then we emit the REAL
        // measured perf event out-of-band so the per-frame stream stays
        // reproducible.
        let sink = self.sink
        runTask = Task { [weak self] in
            for await frame in fs.frames() {
                if Task.isCancelled { break }
                guard let self else { break }
                let events = await self.processFrame(frame)
                for ev in events { await sink.emit(ev) }
                if let perf = await self.perfEventIfDue(at: frame.timestamp) {
                    await sink.emit(perf)
                }
            }
            // Stream finished naturally (e.g. end of file). Mark stopped.
            if let self, !Task.isCancelled {
                await self.finishRun()
            }
        }
    }

    private func finishRun() async {
        state = .stopped
        await emitStatus(message: "capture ended")
        currentSource = nil
    }

    // -----------------------------------------------------------------------
    // read.screen — single-shot OCR readout of ONE captured frame.
    // -----------------------------------------------------------------------

    /// Capture ONE frame from `source` (default .screen), run the OCR (.text)
    /// detector over it, structure the recognized blocks (reading order + control
    /// candidates + optional locator), and emit a vision.screen readout. This is
    /// READ-ON-REQUEST: a single-shot read, NEVER a continuous screen-watch — it
    /// does not touch the live-watch run state (`state`/`currentSource`/`runTask`
    /// are left as-is). Capture goes through the SAME injected FrameSource factory
    /// as watch/analyze, so the real .screen ScreenCaptureKit source is reached in
    /// production (NOT the zero-frame stub) and is DEVICE-GATED (TCC) inside the
    /// source. Honors authorization exactly like beginRun: a denial emits a clean
    /// vision.error + auth status and reads nothing. The recognized text is
    /// SENSITIVE + TRANSIENT (see VisionEvent.screen) — emitted, never persisted.
    public func readScreenOnce(source: CaptureSource, query: String?) async {
        let fs = frameSourceFactory(source)
        let auth = await fs.authorization()
        switch auth {
        case .denied, .restricted:
            await sink.emit(.error(code: "tcc_denied",
                                   message: "screen read not authorized for \(source.tag)",
                                   source: source.tag))
            await emitAuthStatus(source: source, auth: auth)
            return
        case .authorized, .notApplicable, .notDetermined:
            break
        }

        // Pull exactly ONE frame, then tear the stream down (cancelling the
        // consuming task triggers the source's onTermination -> stops capture).
        var captured: Frame?
        let stream = fs.frames()
        for await frame in stream {
            captured = frame
            break
        }

        guard let frame = captured else {
            // Authorized but no frame arrived (e.g. denied-at-start screen source,
            // empty file). Honest: report nothing read rather than fabricate text.
            await sink.emit(.error(code: "no_frame",
                                   message: "no frame captured for screen read",
                                   source: source.tag))
            return
        }

        // OCR-only over the single frame; floor 0 so we surface everything read.
        let dets = detector.detect(in: frame, detectors: .text, minConfidence: 0.0)
        let readout = ScreenStructurer.structure(dets)
        let located = query.flatMap { ScreenStructurer.locate($0, in: readout) }
        await sink.emit(.screen(frameIndex: frame.index, timestamp: frame.timestamp,
                                source: source.tag, readout: readout,
                                located: located, query: query, meta: .screen))
    }

    // -----------------------------------------------------------------------
    // CONTINUOUS SCREEN CONTEXT (#42) — the DEVICE-gated periodic OCR loop that
    // feeds the daemon's bounded/redacted/transient screen-context ring.
    //
    // This is the CONTINUOUS analog of `readScreenOnce`: instead of one read on a
    // voice command, it periodically (every `intervalSeconds`) grabs ONE
    // ScreenCaptureKit frame (TCC-gated, the SAME injected FrameSource seam so it
    // NEVER opens a device on its own), OCRs it, and emits a vision.screen readout
    // tagged `read_kind=context` so the daemon routes it into the context ring (the
    // redaction + bounding + transience all happen daemon-side). A
    // `screen_context.watching` status frames the active window so the HUD shows
    // the prominent WATCHING indicator; when the loop ends it emits watching=false.
    //
    // PRIVACY: OFF by default — this loop is started ONLY by the production socket
    // path when [screen_context].enabled is on (the daemon owns that flag); it is
    // NEVER started by the default-wired or test-wired Pipeline. It is DEVICE-gated
    // (a TCC denial emits a clean error + stops, capturing nothing). The recognized
    // text is SENSITIVE + TRANSIENT (kept off lifelong memory by the daemon, held
    // only in the bounded in-RAM ring). Glyph text only — never a face/identity.
    //
    // HERMETIC: this is wired-not-dead but NOT exercised in tests (no real
    // capture). The contrast test proves the DEFAULT/un-injected Pipeline reaches
    // the zero-frame stub (so a context loop over it captures nothing), exactly the
    // AppWiring discipline — the production wiring is what would flip behavior.
    // -----------------------------------------------------------------------

    /// Run the continuous screen-context loop until cancelled. Each tick captures
    /// ONE frame from `source` (default .screen) through the injected FrameSource
    /// factory, OCRs it, and emits a vision.screen readout tagged `.context`. The
    /// loop honors TCC exactly like `readScreenOnce` (a denial stops it cleanly),
    /// emits a `screen_context.watching` status while active, and emits
    /// watching=false on exit. `maxTicks` bounds the loop for a headless smoke
    /// (nil = run until cancelled in production). DEVICE-gated; OFF by default.
    public func runScreenContextLoop(source: CaptureSource = .screen,
                                     intervalSeconds: Double = 30,
                                     maxTicks: Int? = nil) async {
        // Announce WATCHING (the prominent HUD indicator) — honest about an active
        // continuous capture.
        await sink.emit(.status(state: .watching, source: source.tag,
                                sensitivity: config.sensitivity,
                                cameraAuthorized: nil, screenAuthorized: nil,
                                message: "screen_context.watching"))

        var ticks = 0
        loop: while !Task.isCancelled {
            if let maxTicks, ticks >= maxTicks { break }
            ticks += 1

            let fs = frameSourceFactory(source)
            let auth = await fs.authorization()
            switch auth {
            case .denied, .restricted:
                await sink.emit(.error(code: "tcc_denied",
                                       message: "screen context not authorized for \(source.tag)",
                                       source: source.tag))
                break loop
            case .authorized, .notApplicable, .notDetermined:
                break
            }

            // ONE frame per tick (then tear the stream down).
            var captured: Frame?
            let stream = fs.frames()
            for await frame in stream {
                captured = frame
                break
            }
            if let frame = captured {
                let dets = detector.detect(in: frame, detectors: .text, minConfidence: 0.0)
                let readout = ScreenStructurer.structure(dets)
                // Tagged `.context` so the daemon routes this into the context ring
                // (redacted + bounded + transient daemon-side), distinct from a
                // one-shot read. No locator/query on the continuous path.
                await sink.emit(.screen(frameIndex: frame.index, timestamp: frame.timestamp,
                                        source: source.tag, readout: readout,
                                        located: nil, query: nil, meta: .context))
            }

            if Task.isCancelled { break }
            // Sleep until the next tick. A cancellation during the sleep ends the
            // loop cleanly (CancellationError) — caught so we always emit the
            // watching=false exit status below.
            do {
                // Saturate seconds->nanoseconds so an out-of-range or non-finite
                // interval can never trap Double->UInt64 and crash the app (the IPC
                // decode already clamps; this guards direct callers too). Cap at 1 day.
                let sleepSeconds = intervalSeconds.isFinite ? min(max(intervalSeconds, 0), 86_400) : 0
                try await Task.sleep(nanoseconds: UInt64(sleepSeconds * 1_000_000_000))
            } catch {
                break
            }
        }

        // Honest exit: the loop is no longer watching.
        await sink.emit(.status(state: .stopped, source: source.tag,
                                sensitivity: config.sensitivity,
                                cameraAuthorized: nil, screenAuthorized: nil,
                                message: "screen_context.watching=false"))
    }

    /// START the continuous screen-context loop on its OWN task (the production
    /// `screen.context.start` dispatch). Idempotent: cancels any prior loop first
    /// so a re-start never doubles the WATCHING capture. The loop runs UNBOUNDED
    /// (maxTicks=nil) at `intervalSeconds` until `stopScreenContextLoop` (or a
    /// lifecycle stop) cancels it. OFF by default — only reached when the daemon
    /// sends the op behind [screen_context].enabled. Device-gated at capture.
    public func startScreenContextLoop(source: CaptureSource = .screen,
                                       intervalSeconds: Double = 30) {
        contextTask?.cancel()
        contextTask = Task { [weak self] in
            await self?.runScreenContextLoop(source: source,
                                             intervalSeconds: intervalSeconds,
                                             maxTicks: nil)
        }
    }

    /// STOP the continuous screen-context loop (the `screen.context.stop` dispatch
    /// + the lifecycle-stop teardown). Cancelling the task makes the loop break and
    /// emit its honest watching=false exit. A no-op when no loop is running.
    public func stopScreenContextLoop() {
        contextTask?.cancel()
        contextTask = nil
    }

    // -----------------------------------------------------------------------
    // read.handwriting (#28) — single-shot HANDWRITING/WHITEBOARD read of ONE
    // captured frame. Mirrors read.screen exactly: capture ONE TCC-gated frame
    // through the SAME injected FrameSource factory (so the real .camera/.screen
    // source is reached in production, NOT the zero-frame stub), run the
    // handwriting recognizer (the .text recognizer: .accurate + language
    // correction — the engine default), structure the recognized LINES, and emit
    // a vision.screen readout tagged read_kind=handwriting. READ-ON-REQUEST (does
    // not touch the live-watch run state). The recognized text is SENSITIVE +
    // TRANSIENT. Honest: a TCC denial / no frame reports cleanly; a scrawl that
    // does not read yields an honest empty readout, never a fabricated line.
    // -----------------------------------------------------------------------

    /// Capture ONE frame from `source` (default .camera) and run the handwriting/
    /// whiteboard recognizer over it, emitting a vision.screen readout tagged
    /// read_kind=handwriting. The recognizer is the SAME built-in .text request
    /// (.accurate + usesLanguageCorrection) the screen read uses — the config best
    /// for handwriting — so this routes through the same `detector.detect(.text)`
    /// seam. Honors authorization exactly like readScreenOnce.
    public func readHandwritingOnce(source: CaptureSource) async {
        let fs = frameSourceFactory(source)
        let auth = await fs.authorization()
        switch auth {
        case .denied, .restricted:
            await sink.emit(.error(code: "tcc_denied",
                                   message: "handwriting read not authorized for \(source.tag)",
                                   source: source.tag))
            await emitAuthStatus(source: source, auth: auth)
            return
        case .authorized, .notApplicable, .notDetermined:
            break
        }

        var captured: Frame?
        let stream = fs.frames()
        for await frame in stream {
            captured = frame
            break
        }
        guard let frame = captured else {
            await sink.emit(.error(code: "no_frame",
                                   message: "no frame captured for handwriting read",
                                   source: source.tag))
            return
        }

        // The handwriting recognizer IS the .text recognizer (.accurate +
        // language correction); floor 0 so we surface everything read. An empty
        // readout (a scrawl that did not read) is honest, never fabricated.
        let dets = detector.detect(in: frame, detectors: .text, minConfidence: 0.0)
        let readout = ScreenStructurer.structure(dets)
        await sink.emit(.screen(frameIndex: frame.index, timestamp: frame.timestamp,
                                source: source.tag, readout: readout,
                                located: nil, query: nil,
                                meta: ScreenReadMeta(kind: .handwriting)))
    }

    // -----------------------------------------------------------------------
    // scan.document (#29) — single-shot camera DOCUMENT SCAN of ONE captured
    // frame. Mirrors read.screen's capture: ONE TCC-gated frame through the SAME
    // injected FrameSource factory, then the document scanner
    // (VNDetectDocumentSegmentationRequest -> CIPerspectiveCorrection ->
    // VNRecognizeTextRequest) via the detector seam, and a vision.screen readout
    // tagged read_kind=document carrying the HONEST document_detected bool. When
    // NO document is found, the readout is honestly empty (document_detected=false,
    // no text) — never a fabricated page. READ-ON-REQUEST (does not touch the
    // live-watch run state). The recognized text is SENSITIVE + TRANSIENT. Honest:
    // segmentation/correction QUALITY is device-dependent; capture is TCC-gated.
    // -----------------------------------------------------------------------

    /// Capture ONE frame from `source` (default .camera) and run the document
    /// scanner over it, emitting a vision.screen readout tagged read_kind=document
    /// with the HONEST document_detected bool. When no document is detected the
    /// readout is honestly empty (never a fabricated page). Honors authorization
    /// exactly like readScreenOnce. Routes through the `detector.scanDocument`
    /// seam (the production VisionEngine runs the real segmentation + correction +
    /// OCR; a stub detector finds no document).
    public func scanDocumentOnce(source: CaptureSource) async {
        let fs = frameSourceFactory(source)
        let auth = await fs.authorization()
        switch auth {
        case .denied, .restricted:
            await sink.emit(.error(code: "tcc_denied",
                                   message: "document scan not authorized for \(source.tag)",
                                   source: source.tag))
            await emitAuthStatus(source: source, auth: auth)
            return
        case .authorized, .notApplicable, .notDetermined:
            break
        }

        var captured: Frame?
        let stream = fs.frames()
        for await frame in stream {
            captured = frame
            break
        }
        guard let frame = captured else {
            await sink.emit(.error(code: "no_frame",
                                   message: "no frame captured for document scan",
                                   source: source.tag))
            return
        }

        // Run the document scanner over the single frame: detect quad -> correct
        // -> OCR. When no document is found, `documentDetected` is false and the
        // lines are empty — an honest "no document found", never a fabricated page.
        let scan = detector.scanDocument(in: frame, minConfidence: 0.0)
        let readout = ScreenStructurer.structure(scan.lines)
        await sink.emit(.screen(frameIndex: frame.index, timestamp: frame.timestamp,
                                source: source.tag, readout: readout,
                                located: nil, query: nil,
                                meta: ScreenReadMeta(kind: .document,
                                                     documentDetected: scan.documentDetected)))
    }

    // -----------------------------------------------------------------------
    // describe.capture — capture ONE frame and WRITE it as a PNG for the host's
    // on-device VLM (DISTINCT from read.screen: NO OCR, NO text — just a frame).
    // -----------------------------------------------------------------------

    /// Capture ONE frame from `source` (default .screen) and write it as a PNG to
    /// `path` (the daemon's confined frame location) so the host's on-device VLM
    /// (`infer.describe_image`) can read it. This is the screen-capture REUSE for
    /// the VLM-describe path: it goes through the SAME injected FrameSource factory
    /// as read.screen/watch/analyze (so the real .screen ScreenCaptureKit source
    /// is reached in production, NOT the zero-frame stub), and is DEVICE-GATED
    /// (TCC: Screen Recording) inside the source. Like read.screen it is
    /// READ-ON-REQUEST: a single-shot capture that does NOT touch the live-watch
    /// run state. NO OCR runs — describe.capture emits NO vision.screen readout and
    /// produces NO text; it only hands a pixel frame to the host as a LOCAL file
    /// (pixels never leave the device; the daemon path-confines + re-checks
    /// existence before the op). Honesty: a TCC denial emits a clean tcc_denied
    /// error and writes nothing; no frame emits no_frame; a write failure emits
    /// write_failed — the host then falls back honestly rather than describing a
    /// frame that was never written.
    public func describeCaptureOnce(source: CaptureSource, path: String) async {
        let fs = frameSourceFactory(source)
        let auth = await fs.authorization()
        switch auth {
        case .denied, .restricted:
            await sink.emit(.error(code: "tcc_denied",
                                   message: "screen capture not authorized for \(source.tag)",
                                   source: source.tag))
            await emitAuthStatus(source: source, auth: auth)
            return
        case .authorized, .notApplicable, .notDetermined:
            break
        }

        // Pull exactly ONE frame, then tear the stream down (cancelling the
        // consuming task triggers the source's onTermination -> stops capture).
        var captured: Frame?
        let stream = fs.frames()
        for await frame in stream {
            captured = frame
            break
        }

        guard let frame = captured else {
            // Authorized but no frame arrived. Honest: report nothing captured
            // rather than write a stale/blank PNG the VLM would then "describe".
            await sink.emit(.error(code: "no_frame",
                                   message: "no frame captured for describe",
                                   source: source.tag))
            return
        }

        // Normalize the frame to a CGImage and write it as a PNG at the confined
        // path. The pixels are written to a LOCAL file and never leave the device.
        guard let img = VisionEngine.encodableCGImage(for: frame),
              VisionEngine.writeCGImagePNG(img, to: path) else {
            await sink.emit(.error(code: "write_failed",
                                   message: "could not write the captured frame for describe",
                                   source: source.tag))
            return
        }

        // Success: the frame is written. Report a status (NO pixels, NO path text
        // beyond the source tag) so the host/HUD sees the capture completed; the
        // host's existence check + path-confinement guard the actual file.
        await emitStatus(message: "frame captured for describe")
    }

    // -----------------------------------------------------------------------
    // classify.sound — single-shot Sound Analysis over ONE supplied audio clip.
    // -----------------------------------------------------------------------

    /// Classify the audio clip at `path` with the built-in Sound Analysis
    /// classifier (SNClassifySoundRequest, the ~300-class version1) and emit a
    /// vision.sound readout with the top sound classes {label, confidence}. This
    /// is the "what was that sound" / identify-sound path. The clip is supplied by
    /// the host (the daemon wrote it from its captured buffer) — this op NEVER
    /// opens the mic and NEVER does continuous capture: it decodes the file
    /// locally, classifies it on-device, and discards the audio. It does NOT touch
    /// the live-watch run state (a one-shot, like read.screen/describe.capture).
    /// PRIVACY: ONLY the derived sound-class LABELS are emitted — the AUDIO ITSELF
    /// NEVER LEAVES the device. DISTINCT from STT (speech): no transcript is
    /// produced. Honesty: a missing/corrupt clip or a classifier that returned
    /// nothing (e.g. a clip shorter than the ~3s window) emits a clean
    /// vision.error rather than fabricating labels.
    public func classifySoundOnce(path: String) async {
        let classes = soundClassifier.classify(
            audioClipPath: path, minConfidence: config.minConfidence)
        guard !classes.isEmpty else {
            // No labels: either the clip could not be decoded, or the classifier
            // produced nothing over it (too short / silence). Honest: report
            // nothing classified rather than invent a sound class.
            await sink.emit(.error(code: "no_sound_classes",
                                   message: "no sound classes for the supplied clip",
                                   source: "sound"))
            return
        }
        await sink.emit(.sound(timestamp: Date().timeIntervalSince1970,
                               source: "sound",
                               classes: classes,
                               classifier: SoundEngine.classifierTag,
                               computeUnit: computeUnitTag))
    }

    private func stopRun(to newState: VisionEvent.WatchState) async {
        runTask?.cancel()
        runTask = nil
        state = newState
        currentSource = nil
        await emitStatus(message: nil)
    }

    // -----------------------------------------------------------------------
    // Per-frame processing — PURE given (detector, config, internal state).
    // Returns the events that should be emitted for this frame, in order. This
    // is the seam the unit tests drive directly (no source, no sink, no clock).
    // -----------------------------------------------------------------------

    /// Process one frame: run the detector, derive motion vs the previous frame,
    /// update presence + debounce, and return the resulting VisionEvents in the
    /// order they should be emitted. Increments the frame index.
    @discardableResult
    public func processFrame(_ frame: Frame) -> [VisionEvent] {
        let idx = frameIndex
        frameIndex += 1
        var out: [VisionEvent] = []
        let srcTag = (currentSource ?? frame.source).tag

        // 1. Inference (built-in detectors via the seam). MEASURED: detectTimed
        //    brackets the real on-device Vision `perform` with a monotonic clock,
        //    so `inferenceMs` is a genuine per-frame inference latency.
        let (detections, inferenceMs) = detector.detectTimed(
            in: frame, detectors: config.detectors, minConfidence: config.minConfidence)

        // 2. Motion from frame-to-frame luma delta (defensive: derived stat).
        var motionResult = MotionResult.none
        if let grid = LumaGrid.sample(frame) {
            motionResult = motion.step(grid, threshold: config.motionThreshold)
        }

        // 3. Presence evidence = max(detection confidence, motion magnitude).
        let topConfidence = detections.map(\.confidence).max() ?? 0
        let evidence = max(topConfidence, motionResult.magnitude)
        let presenceChanged = presence.update(evidence: evidence)

        // 4. Emit a detections event (debounced) when there are detections OR
        //    presence just changed (so the HUD sees enter/exit promptly).
        if !detections.isEmpty {
            if presenceChanged || detectionDebounce.admit(at: frame.timestamp) == .emit {
                if presenceChanged { _ = detectionDebounce.admit(at: frame.timestamp) }
                out.append(.detections(frameIndex: idx, timestamp: frame.timestamp,
                                       source: srcTag, detections: detections))
            }
        }

        // 5. Emit a motion event (debounced) when motion crossed threshold.
        if motionResult.exceeded,
           motionDebounce.admit(at: frame.timestamp) == .emit {
            out.append(.motion(frameIndex: idx, timestamp: frame.timestamp, source: srcTag,
                               magnitude: motionResult.magnitude, region: motionResult.region))
        }

        // 6. A presence transition is a status update (sticky, anti-flicker).
        if presenceChanged {
            out.append(.status(state: state, source: srcTag,
                               sensitivity: config.sensitivity,
                               cameraAuthorized: nil, screenAuthorized: nil,
                               message: presence.state == .present ? "present" : "absent"))
        }

        // 7. Record the REAL measured inference latency for perf telemetry. We
        //    only sample when inference actually ran this frame (inferenceMs > 0;
        //    a backing-less frame does no work and is not a sample). NOTE: the
        //    measured ms is wall-clock and therefore NON-deterministic, so it is
        //    deliberately NOT returned in `out` (which must stay reproducible for
        //    deterministic-replay tests). The perf EVENT is emitted out-of-band by
        //    the run loop via `perfEventIfDue(at:)` — see `beginRun`.
        if inferenceMs > 0 {
            inferenceMsSamples.append(inferenceMs)
        }

        return out
    }

    /// Build a vision.perf event from the REAL measured inference-ms samples seen
    /// so far this run, IF the perf debounce admits at `time` (so high-fps capture
    /// doesn't spam). Returns nil when nothing has been measured yet or the
    /// debounce suppresses. p50/p95 are genuine measured latencies; `fps` is the
    /// INFERENCE-bound throughput ceiling derived from p50 (1000/p50) — NOT a
    /// measured real-time capture/camera rate (that is device-gated). Called from
    /// the run loop AFTER processFrame so the non-deterministic timing stays out
    /// of the pure per-frame event stream.
    public func perfEventIfDue(at time: TimeInterval) -> VisionEvent? {
        guard !inferenceMsSamples.isEmpty else { return nil }
        guard perfDebounce.admit(at: time) == .emit else { return nil }
        let p50 = Self.percentile(inferenceMsSamples, 0.50)
        let p95 = Self.percentile(inferenceMsSamples, 0.95)
        let inferenceFps = p50 > 0 ? 1000.0 / p50 : 0
        return .perf(p50Ms: p50, p95Ms: p95, fps: inferenceFps,
                     frames: UInt64(inferenceMsSamples.count),
                     computeUnit: computeUnitTag)
    }

    /// Nearest-rank percentile over the measured inference-ms samples (q in
    /// 0...1). Returns 0 for an empty set. Pure + deterministic.
    static func percentile(_ samples: [Double], _ q: Double) -> Double {
        guard !samples.isEmpty else { return 0 }
        let sorted = samples.sorted()
        let clampedQ = max(0, min(1, q))
        let rank = Int((clampedQ * Double(sorted.count - 1)).rounded())
        return sorted[max(0, min(sorted.count - 1, rank))]
    }

    /// Test/inspection helper: current watch state.
    public var currentState: VisionEvent.WatchState { state }
    /// Test/inspection helper: current presence.
    public var currentPresence: Presence { presence.state }
    /// Test/inspection helper: current effective config.
    public var currentConfig: PipelineConfig { config }

    // -----------------------------------------------------------------------
    // Status emission.
    // -----------------------------------------------------------------------

    private func emitStatus(message: String?) async {
        await sink.emit(.status(
            state: state,
            source: currentSource?.tag,
            sensitivity: config.sensitivity,
            cameraAuthorized: nil,
            screenAuthorized: nil,
            message: message
        ))
    }

    private func emitAuthStatus(source: CaptureSource, auth: CaptureAuthorization) async {
        let authorized: Bool? = {
            switch auth {
            case .authorized, .notApplicable: return true
            case .denied, .restricted: return false
            case .notDetermined: return nil
            }
        }()
        let camAuth: Bool? = source == .camera ? authorized : nil
        let scrAuth: Bool? = source == .screen ? authorized : nil
        await sink.emit(.status(
            state: state,
            source: source.tag,
            sensitivity: config.sensitivity,
            cameraAuthorized: camAuth,
            screenAuthorized: scrAuth,
            message: nil
        ))
    }
}
