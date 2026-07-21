// VisionEvent.swift — the APP -> HOST telemetry contract (module agents AND the
// HUD agent build against it). The items/status/log surface is FROZEN — do not
// change it. The `modules` type below is a DELIBERATE, ADDITIVE extension (2026-07,
// docs/INTROSPECT.md): it adds a new relay type without touching any existing case
// or its serialization, and it mirrors the daemon's GENERIC `modules` handling
// (daemon/src/apps.rs::classify_inbound_line already routes `modules` to the
// introspection attestor, not to a vision.* topic), so it needs no daemon change
// and cannot alter how the frozen events are relayed.
//
// WIRE FRAMING (daemon/src/apps.rs::relay_line / classify_inbound_line):
// every app->host line is one JSON object:
//     {"token": <hex>, "type": "items"|"status"|"log"|"modules", "data": <obj|str>}
//   - token is verified on EVERY line (HMAC-SHA256); a bad/missing token ->
//     line dropped + app.auth_failed.
//   - type "items" or "status" -> relayed as telemetry event "app.data" with
//     payload {"name":"vision","topic":<topic>,"payload":<data>}.
//   - the relay TOPIC = data["topic"] IF it is one this app DECLARED in
//     manifest.toml `telemetry_topics`, else the first declared topic. So every
//     VisionEvent's `data` MUST carry a "topic" set to a vision.* value, or it
//     silently lands on the default topic (vision.detections).
//   - type "log" -> app.log {"name","line"}.
//
// IMPORTANT (defensive): `data` carries COUNTS, BOUNDING BOXES, LABELS, TIMING
// — never image bytes, never an identity. Frames never leave the device.
//
// This file owns: (1) the vision.* topic constants, (2) the typed event
// payloads, (3) the envelope that stamps the token + frames a line as the
// daemon expects. The ipc module just calls `line(token:)`.

import Foundation

/// The vision.* telemetry topics (must match manifest.toml exactly).
public enum VisionTopic {
    public static let detections = "vision.detections"  // default topic
    public static let status     = "vision.status"
    public static let motion     = "vision.motion"
    public static let perf       = "vision.perf"
    public static let error      = "vision.error"
    public static let screen     = "vision.screen"      // OCR screen-read readout
    public static let sound      = "vision.sound"       // Sound Analysis class readout (LABELS only)

    /// All declared topics, in manifest order (first = default).
    public static let all = [detections, status, motion, perf, error, screen, sound]
}

/// Which read produced a vision.screen readout — the screen OCR (read.screen),
/// the handwriting/whiteboard recognizer (#28, read.handwriting), or the camera
/// document scanner (#29, scan.document). Carried on the vision.screen event so
/// the HUD readout can label the read honestly without a separate topic.
public enum ScreenReadKind: String, Sendable, Equatable {
    case screen       // read.screen — on-screen text OCR
    case handwriting  // read.handwriting — handwriting / whiteboard recognizer (#28)
    case document     // scan.document — camera document scanner (#29)
    case context      // continuous screen-context loop (#42) — periodic on-screen
                      // OCR snapshot fed to the daemon's bounded/redacted/transient
                      // ring. Same OCR engine as `screen`; the distinct kind lets
                      // the daemon route the snapshot into the context ring (and the
                      // HUD show the WATCHING indicator) rather than treat it as a
                      // one-shot read. DEVICE-gated + OFF by default.
}

/// NON-RAW-TEXT metadata for a vision.screen readout. Carries the read KIND, the
/// recognized-text PRESENCE + LENGTH (a length integer + a bool, NOT the text
/// itself — so a status-style surface can show "did it read anything / how much"
/// without the sensitive glyphs), and the HONEST document-detected bool for the
/// scanner (#29). The raw recognized text still rides the existing `text`/`blocks`
/// fields of the same items event (transient, kept off lifelong memory); THIS
/// struct is the safe-to-summarize signal. `documentDetected` is nil for the
/// non-document reads (screen/handwriting), where document detection is N/A.
public struct ScreenReadMeta: Sendable, Equatable {
    /// Which read produced this readout (screen OCR / handwriting / document).
    public let kind: ScreenReadKind
    /// HONEST document-detected bool for the scanner (#29). nil for screen/
    /// handwriting reads (document detection does not apply). When the scanner
    /// found NO page this is false and the readout is honestly empty.
    public let documentDetected: Bool?
    /// SCREEN GROUNDING: the FRONTMOST app at the capture instant ("Terminal") —
    /// the whole-display OCR may include other visible windows' text alongside it,
    /// read AX-free (NSWorkspace + the capture's own Screen-Recording consent —
    /// see FrontmostReader). nil = honestly unknown (headless/unattributed),
    /// NEVER fabricated. Applies to screen reads only; camera/file reads carry
    /// nil (a frontmost app says nothing about a camera frame).
    public let sourceApp: String?
    /// The frontmost window TITLE, when readable. Titles can carry sensitive
    /// text (subject lines, document names) — the daemon REDACTS before store,
    /// exactly like the OCR text itself. nil = honestly unknown.
    public let sourceWindow: String?

    public init(kind: ScreenReadKind, documentDetected: Bool? = nil,
                sourceApp: String? = nil, sourceWindow: String? = nil) {
        self.kind = kind
        self.documentDetected = documentDetected
        self.sourceApp = sourceApp
        self.sourceWindow = sourceWindow
    }

    /// The default meta for the plain screen OCR read (read.screen).
    public static let screen = ScreenReadMeta(kind: .screen, documentDetected: nil)

    /// The meta for a CONTINUOUS screen-context snapshot (#42) — same OCR as the
    /// plain screen read, tagged so the daemon routes it into the bounded/redacted/
    /// transient context ring (and the HUD shows the WATCHING indicator).
    public static let context = ScreenReadMeta(kind: .context, documentDetected: nil)
}

/// The daemon `type` field for an app->host line.
public enum RelayType: String, Sendable {
    case items    // -> app.data
    case status   // -> app.data
    case log      // -> app.log
    case modules  // -> introspect module attestation (docs/INTROSPECT.md)
}

/// A typed Vision telemetry event. `encodeData()` produces the `data` object
/// (always including its `topic`); `line(token:)` produces the full
/// token-stamped JSONL line the app writes to its socket.
public enum VisionEvent: Sendable {

    /// vision.detections — per-frame detection summary. `type:"items"`.
    /// Payload: a frame index/timestamp, the source tag, total count, a
    /// per-kind count breakdown, and the detections themselves (bounding boxes
    /// + labels + confidence — NO pixels, NO identity).
    case detections(frameIndex: UInt64, timestamp: TimeInterval, source: String,
                    detections: [Detection])

    /// vision.status — watch lifecycle / capability snapshot. `type:"status"`.
    case status(state: WatchState, source: String?, sensitivity: Double,
                cameraAuthorized: Bool?, screenAuthorized: Bool?, message: String?)

    /// vision.motion — a motion event (change region crossing threshold).
    /// `type:"items"`.
    case motion(frameIndex: UInt64, timestamp: TimeInterval, source: String,
                magnitude: Double, region: DetectionBox)

    /// vision.perf — inference timing. `type:"status"`.
    case perf(p50Ms: Double, p95Ms: Double, fps: Double, frames: UInt64, computeUnit: String)

    /// vision.error — a recoverable error. `type:"status"`.
    case error(code: String, message: String, source: String?)

    /// vision.screen — the OCR screen-read readout (the .readScreen op result).
    /// `type:"items"`. Carries the recognized text blocks (string + box +
    /// confidence), the full readable text in reading order, the candidate
    /// control labels, and (optionally) the located block for a "where is <query>"
    /// request. PRIVACY: the recognized text is SENSITIVE + TRANSIENT — it may
    /// contain on-screen passwords/messages, so the host must NOT persist it to
    /// lifelong memory/optimizer traces by default. Pixels never leave the device;
    /// only this derived text/box readout crosses the socket.
    case screen(frameIndex: UInt64, timestamp: TimeInterval, source: String,
                readout: ScreenReadout, located: ScreenStructurer.Located?, query: String?,
                meta: ScreenReadMeta)

    /// vision.sound — the Sound Analysis class readout (the .classifySound op
    /// result, and the daemon's opt-in ambient monitor). `type:"items"`. Carries
    /// the top sound classes (label + confidence) from the built-in ~300-class
    /// classifier, the classifier tag (so the consumer knows it is the fixed
    /// version1 vocabulary, not "any sound"), and the compute-unit tag. PRIVACY:
    /// ONLY the derived LABELS cross the socket — the AUDIO ITSELF NEVER LEAVES the
    /// device; there is deliberately no audio field. This is audio SCENE
    /// understanding (dog bark / doorbell / alarm / music), DISTINCT from STT
    /// (speech) — no transcript is produced.
    case sound(timestamp: TimeInterval, source: String, classes: [SoundClass],
               classifier: String, computeUnit: String)

    /// modules — the READ-ONLY dyld module self-report (docs/INTROSPECT.md).
    /// `type:"modules"` (NOT relayed as app.data — the daemon routes it to the
    /// introspection attestor, not to a vision.* topic). Carries only image PATHS
    /// + LC_UUIDs; no pixels, no identity. This is an ADDITIVE extension of the
    /// contract (see the header note) that mirrors the daemon's generic `modules`
    /// message type, so it needs no daemon change.
    case modules([DyldModule])

    /// The watch lifecycle states reported on vision.status.
    public enum WatchState: String, Sendable, Codable {
        case idle
        case watching
        case analyzing
        case stopped
    }

    /// The daemon `type` for this event (items vs status).
    public var relayType: RelayType {
        switch self {
        case .detections, .motion, .screen, .sound: return .items
        case .status, .perf, .error: return .status
        case .modules: return .modules
        }
    }

    /// The vision.* topic this event targets.
    public var topic: String {
        switch self {
        case .detections: return VisionTopic.detections
        case .status:     return VisionTopic.status
        case .motion:     return VisionTopic.motion
        case .perf:       return VisionTopic.perf
        case .error:      return VisionTopic.error
        case .screen:     return VisionTopic.screen
        case .sound:      return VisionTopic.sound
        // modules is NOT topic-routed (the daemon handles the `modules` type
        // before topic resolution); return empty so the switch stays exhaustive.
        case .modules:    return ""
        }
    }

    /// Build the `data` object for this event. ALWAYS includes `topic` so the
    /// daemon relays it onto the right vision.* channel.
    public func encodeData() -> [String: Any] {
        var d: [String: Any] = ["topic": topic]
        switch self {
        case let .detections(frameIndex, timestamp, source, detections):
            d["frame"] = frameIndex
            d["ts"] = timestamp
            d["source"] = source
            d["count"] = detections.count
            d["by_kind"] = countByKind(detections)
            d["detections"] = detections.map(encodeDetection)

        case let .status(state, source, sensitivity, cameraAuthorized, screenAuthorized, message):
            d["state"] = state.rawValue
            if let source { d["source"] = source }
            d["sensitivity"] = sensitivity
            if let cameraAuthorized { d["camera_authorized"] = cameraAuthorized }
            if let screenAuthorized { d["screen_authorized"] = screenAuthorized }
            if let message { d["message"] = message }

        case let .motion(frameIndex, timestamp, source, magnitude, region):
            d["frame"] = frameIndex
            d["ts"] = timestamp
            d["source"] = source
            d["magnitude"] = magnitude
            d["region"] = encodeBox(region)

        case let .perf(p50Ms, p95Ms, fps, frames, computeUnit):
            d["p50_ms"] = p50Ms
            d["p95_ms"] = p95Ms
            d["fps"] = fps
            d["frames"] = frames
            d["compute_unit"] = computeUnit

        case let .error(code, message, source):
            d["code"] = code
            d["message"] = message
            if let source { d["source"] = source }

        case let .screen(frameIndex, timestamp, source, readout, located, query, meta):
            d["frame"] = frameIndex
            d["ts"] = timestamp
            d["source"] = source
            d["block_count"] = readout.blocks.count
            // The full readable text (reading order). SENSITIVE + TRANSIENT.
            d["text"] = readout.fullText
            d["blocks"] = readout.blocks.map(encodeBlock)
            d["controls"] = readout.controls.map(encodeBlock)
            if let query { d["query"] = query }
            if let located {
                var loc = encodeBlock(located.block)
                loc["score"] = located.score
                d["located"] = loc
            }
            // NON-RAW-TEXT signal (safe for a status-style readout): which read
            // KIND produced this, whether ANY text was recognized + how MUCH
            // (presence + length, NOT the glyphs), and the HONEST document-detected
            // bool for the scanner (#29). The raw text rides `text`/`blocks` above
            // (transient); these summarize it without exposing the sensitive
            // content. text_present/text_length let the HUD show "read N chars"
            // without rendering passwords/messages.
            d["read_kind"] = meta.kind.rawValue
            d["text_present"] = !readout.fullText.isEmpty
            d["text_length"] = readout.fullText.count
            if let documentDetected = meta.documentDetected {
                d["document_detected"] = documentDetected
            }
            // SCREEN GROUNDING (additive, the sanctioned VisionEvent extension
            // pattern): which app/window was FRONTMOST at capture. Omitted when
            // unknown — absence is honest, a key is never fabricated. The title
            // may be sensitive; the daemon redacts before store (like `text`).
            if let sourceApp = meta.sourceApp { d["source_app"] = sourceApp }
            if let sourceWindow = meta.sourceWindow { d["source_window"] = sourceWindow }

        case let .sound(timestamp, source, classes, classifier, computeUnit):
            d["ts"] = timestamp
            d["source"] = source
            d["count"] = classes.count
            // ONLY labels + confidence — NEVER audio. The classifier tag makes the
            // fixed ~300-class vocabulary explicit (not "any sound"); the
            // compute_unit tag mirrors vision.perf (ANE/GPU-eligible placement).
            d["classes"] = classes.map { ["label": $0.label, "confidence": $0.confidence] }
            d["classifier"] = classifier
            d["compute_unit"] = computeUnit

        case let .modules(mods):
            // NOT topic-routed — return {"modules":[{"path","uuid"?}]} WITHOUT the
            // topic key (the daemon's parse_module_report reads data.modules). uuid
            // is omitted when nil (JSON has no null-friendly path here and the
            // daemon treats an absent uuid as None).
            return ["modules": mods.map { m -> [String: Any] in
                var e: [String: Any] = ["path": m.path]
                if let u = m.uuid { e["uuid"] = u }
                return e
            }]
        }
        return d
    }

    /// The full token-stamped JSONL line (no trailing newline; the ipc writer
    /// appends one). Returns nil only if JSON serialization fails (never for
    /// the shapes here, but the API is non-throwing-friendly).
    public func line(token: String) -> String? {
        let envelope: [String: Any] = [
            "token": token,
            "type": relayType.rawValue,
            "data": encodeData(),
        ]
        guard JSONSerialization.isValidJSONObject(envelope),
              let data = try? JSONSerialization.data(withJSONObject: envelope, options: [.sortedKeys]),
              let s = String(data: data, encoding: .utf8)
        else { return nil }
        return s
    }

    // --- encoding helpers ---

    private func encodeDetection(_ det: Detection) -> [String: Any] {
        [
            "kind": det.kind.rawValue,
            "box": encodeBox(det.boundingBox),
            "confidence": det.confidence,
            "label": det.label,
        ]
    }

    private func encodeBox(_ b: DetectionBox) -> [String: Any] {
        ["x": b.x, "y": b.y, "w": b.width, "h": b.height]
    }

    /// Encode one screen-readout text block: the recognized string, its box, its
    /// center (Vision coords), confidence, and the control-candidate flag. The
    /// center is a "where" for describing/locating a control — NOT a click point.
    private func encodeBlock(_ b: ScreenReadout.Block) -> [String: Any] {
        [
            "text": b.text,
            "box": encodeBox(b.box),
            "center": ["x": b.center.x, "y": b.center.y],
            "confidence": b.confidence,
            "is_control": b.isControlCandidate,
        ]
    }

    private func countByKind(_ dets: [Detection]) -> [String: Int] {
        var counts: [String: Int] = [:]
        for d in dets { counts[d.kind.rawValue, default: 0] += 1 }
        return counts
    }
}
