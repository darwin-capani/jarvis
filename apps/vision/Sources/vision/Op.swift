// Op.swift — the HOST -> APP command vocabulary (FROZEN; module agents build
// against it, must not change it).
//
// The daemon forwards two kinds of host->app JSONL lines on the app socket
// (see daemon/src/apps.rs::send_command / send_op_line):
//   1. control verbs:  {"type":"start"|"refresh"|"stop"}
//   2. op lines (verbatim from the router): the daemon does NOT interpret the
//      body — the op contract lives HERE, in the app.
//
// We model BOTH as `Op` so the ipc module decodes every inbound line through
// one door. The on-wire shape for an op is:
//     {"type":"op","op":"<name>", ...op-specific fields... }
// and for a control verb the bare {"type":"start|refresh|stop"}.
//
// Decoding is total: an unknown/malformed line decodes to `.unknown(raw:)` so
// the app can drop it + emit vision.error rather than crash.

import Foundation

/// A command the app received from the host.
public enum Op: Sendable, Equatable {
    // --- control verbs (host lifecycle) ---
    case start
    case refresh
    case stop

    // --- vision ops (the app's own contract) ---
    /// watch.start {source}: begin live analysis of a capture source.
    case watchStart(source: CaptureSource)
    /// watch.stop: end the current live watch.
    case watchStop
    /// analyze.file {path}: analyze a video file under videos/input.
    case analyzeFile(path: String)
    /// set.sensitivity {value}: set detection/motion sensitivity in 0...1.
    case setSensitivity(value: Double)
    /// status: emit a vision.status snapshot.
    case status
    /// read.screen {source?}: capture ONE frame from a source (default .screen),
    /// run the OCR (.text) detector, and emit a vision.screen readout with the
    /// recognized text blocks + boxes + structuring. ADDITIVE (the frozen op
    /// shapes above are untouched). READ-ON-REQUEST: a single-shot read, never a
    /// continuous screen-watch. DEVICE-GATED at capture (TCC: Screen Recording).
    case readScreen(source: CaptureSource)

    /// describe.capture {path, source?}: capture ONE frame from a source (default
    /// .screen) and WRITE it as a PNG to `path` (the daemon's confined frame
    /// location) so the on-device VLM (`infer.describe_image`) can read it. This is
    /// the screen-capture REUSE for the VLM-describe path — DISTINCT from
    /// read.screen (which runs OCR + emits text): describe.capture produces NO
    /// text/OCR, it only hands a pixel frame to the host's VLM. The pixels are
    /// written to a LOCAL file under the daemon's project root and never leave the
    /// device. ADDITIVE. DEVICE-GATED at capture (TCC: Screen Recording). The
    /// `path` is REQUIRED (the host names the confined output); an explicit source
    /// (incl. .file) is accepted so this is headlessly testable like read.screen.
    case describeCapture(path: String, source: CaptureSource)

    /// classify.sound {path}: classify an AUDIO CLIP at `path` (a wav/audio file
    /// the daemon wrote from its captured buffer) with the built-in Sound
    /// Analysis classifier (SNClassifySoundRequest, the ~300-class
    /// SNClassifierIdentifier.version1) and emit a vision.sound readout with the
    /// top sound classes {label, confidence}. ADDITIVE (the frozen op shapes above
    /// are untouched). This is the "what was that sound" / identify-sound path:
    /// audio SCENE understanding (dog bark / doorbell / alarm / music), DISTINCT
    /// from STT (speech). On-device + offline; ONLY the derived sound-class LABELS
    /// cross the socket — the AUDIO ITSELF NEVER LEAVES the device. `path` is
    /// REQUIRED (the host names the confined clip); a classify.sound WITHOUT a path
    /// is malformed -> .unknown (the app refuses to classify without a clip). NOTE:
    /// continuous live mic monitoring is a SEPARATE, opt-in + TCC-gated daemon
    /// concern — this op classifies ONE supplied clip, it never opens the mic.
    case classifySound(path: String)

    /// read.handwriting {source?}: capture ONE frame from a source (default
    /// .camera — handwriting/whiteboard is most naturally read off the camera; an
    /// explicit .screen or .file is accepted) and run the HANDWRITING recognizer
    /// (VNRecognizeTextRequest, .accurate + language correction — the config best
    /// for handwriting/whiteboard text), emitting a vision.screen readout with the
    /// recognized LINES + boxes (same shape as read.screen). ADDITIVE (#28). READ-
    /// ON-REQUEST: a single-shot read, never a continuous watch. DEVICE-GATED at
    /// capture (TCC: Camera/Screen). Source is OPTIONAL: absent -> .camera. The
    /// recognized text is SENSITIVE + TRANSIENT (kept off lifelong memory). Honest:
    /// recognition QUALITY is device/Vision-model-dependent; a scrawl may not read.
    case readHandwriting(source: CaptureSource)

    /// scan.document {source?}: capture ONE frame from a source (default .camera —
    /// a document is scanned with the camera; an explicit .screen or .file is
    /// accepted) and run the DOCUMENT SCANNER (VNDetectDocumentSegmentationRequest
    /// -> CIPerspectiveCorrection -> VNRecognizeTextRequest), emitting a
    /// vision.screen readout with the recognized text off the CORRECTED page plus
    /// the HONEST document-detected bool. When NO document is found, the readout is
    /// honestly empty (never a fabricated page). ADDITIVE (#29). READ-ON-REQUEST: a
    /// single-shot scan, never a continuous watch. DEVICE-GATED at capture (TCC:
    /// Camera). Source is OPTIONAL: absent -> .camera. The recognized text is
    /// SENSITIVE + TRANSIENT. Honest: segmentation/correction QUALITY is device-
    /// dependent; live camera capture is TCC-gated. DEFENSIVE: glyph text only,
    /// never a face/person id.
    case scanDocument(source: CaptureSource)

    /// screen.context.start {source?, interval_secs?}: BEGIN the CONTINUOUS screen-
    /// context loop (#42) — periodically (every `interval_secs`, default 30s) grab
    /// ONE frame from `source` (default .screen) through the SAME injected
    /// FrameSource seam, OCR it, and emit a vision.screen readout tagged
    /// read_kind=context (which the daemon routes into its bounded/redacted/
    /// transient context ring). Emits a `screen_context.watching` status while
    /// active (the prominent HUD WATCHING indicator) and an honest watching=false
    /// exit on stop. ADDITIVE (the frozen op shapes above are untouched). OFF BY
    /// DEFAULT: the daemon sends this ONLY when [screen_context].enabled is on, and
    /// the loop is DEVICE-GATED at capture (TCC: Screen Recording) — a denial stops
    /// it cleanly, capturing nothing. The recognized text is SENSITIVE + TRANSIENT
    /// (kept off lifelong memory; held only in the in-RAM ring). Glyph text only —
    /// never a face/person id.
    case screenContextStart(source: CaptureSource, intervalSecs: Double)

    /// screen.context.stop: END the continuous screen-context loop (cancels the
    /// loop task, which emits the honest watching=false exit). ADDITIVE. The
    /// recall/forget of the ring is a daemon-side concern — this op only stops the
    /// capture loop.
    case screenContextStop

    /// A line we could not classify — kept so the app can drop + report it.
    case unknown(raw: String)

    /// The canonical op name string as it appears on the wire ("op" field), or
    /// the control verb string for the lifecycle cases. `unknown` has none.
    public var wireName: String? {
        switch self {
        case .start:          return "start"
        case .refresh:        return "refresh"
        case .stop:           return "stop"
        case .watchStart:     return "watch.start"
        case .watchStop:      return "watch.stop"
        case .analyzeFile:    return "analyze.file"
        case .setSensitivity: return "set.sensitivity"
        case .status:         return "status"
        case .readScreen:     return "read.screen"
        case .describeCapture: return "describe.capture"
        case .classifySound:  return "classify.sound"
        case .readHandwriting: return "read.handwriting"
        case .scanDocument:    return "scan.document"
        case .screenContextStart: return "screen.context.start"
        case .screenContextStop:  return "screen.context.stop"
        case .unknown:        return nil
        }
    }
}

extension Op {
    /// Decode one already-parsed JSON object into an Op. Total: anything that
    /// does not match a known shape becomes `.unknown(raw:)`.
    ///
    /// `raw` is the original line text, carried into `.unknown` for diagnostics.
    public static func decode(json: [String: Any], raw: String) -> Op {
        let type = (json["type"] as? String) ?? ""
        switch type {
        case "start":   return .start
        case "refresh": return .refresh
        case "stop":    return .stop
        case "op":
            let name = (json["op"] as? String) ?? ""
            return decodeOp(name: name, json: json, raw: raw)
        default:
            return .unknown(raw: raw)
        }
    }

    /// Decode one raw JSONL line into an Op (parses JSON then dispatches).
    public static func decode(line: String) -> Op {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              let data = trimmed.data(using: .utf8),
              let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        else {
            return .unknown(raw: line)
        }
        return decode(json: obj, raw: line)
    }

    private static func decodeOp(name: String, json: [String: Any], raw: String) -> Op {
        switch name {
        case "watch.start":
            guard let src = decodeSource(json: json) else { return .unknown(raw: raw) }
            return .watchStart(source: src)
        case "watch.stop":
            return .watchStop
        case "analyze.file":
            guard let path = json["path"] as? String, !path.isEmpty else { return .unknown(raw: raw) }
            return .analyzeFile(path: path)
        case "set.sensitivity":
            guard let v = (json["value"] as? Double) ?? (json["value"] as? NSNumber)?.doubleValue
            else { return .unknown(raw: raw) }
            return .setSensitivity(value: v)
        case "status":
            return .status
        case "read.screen":
            // Source is OPTIONAL: absent -> .screen (the read-my-screen default).
            // An explicit source (incl. .file) is accepted so the OCR read path
            // is headlessly testable over a confined file, like analyze.file.
            if json["source"] == nil {
                return .readScreen(source: .screen)
            }
            guard let src = decodeSource(json: json) else { return .unknown(raw: raw) }
            return .readScreen(source: src)
        case "describe.capture":
            // `path` is REQUIRED — the host names the confined PNG output the app
            // must write. Source is OPTIONAL: absent -> .screen (the
            // describe-my-screen default). An explicit source (incl. .file) is
            // accepted so the capture-and-write path is headlessly testable, like
            // read.screen. A describe.capture WITHOUT a path is malformed ->
            // .unknown (the app refuses to capture without a write target).
            guard let path = json["path"] as? String, !path.isEmpty else { return .unknown(raw: raw) }
            if json["source"] == nil {
                return .describeCapture(path: path, source: .screen)
            }
            guard let src = decodeSource(json: json) else { return .unknown(raw: raw) }
            return .describeCapture(path: path, source: src)
        case "classify.sound":
            // `path` is REQUIRED — the host names the confined audio clip the app
            // must classify. A classify.sound WITHOUT a path is malformed ->
            // .unknown (the app refuses to classify without a clip; it never opens
            // the mic itself). Mirrors describe.capture's path requirement.
            guard let path = json["path"] as? String, !path.isEmpty else { return .unknown(raw: raw) }
            return .classifySound(path: path)
        case "read.handwriting":
            // Source is OPTIONAL: absent -> .camera (handwriting/whiteboard is
            // most naturally read off the camera). An explicit source (incl.
            // .file) is accepted so the handwriting read path is headlessly
            // testable over a confined file, like read.screen.
            if json["source"] == nil {
                return .readHandwriting(source: .camera)
            }
            guard let src = decodeSource(json: json) else { return .unknown(raw: raw) }
            return .readHandwriting(source: src)
        case "scan.document":
            // Source is OPTIONAL: absent -> .camera (a document is scanned with
            // the camera). An explicit source (incl. .file) is accepted so the
            // scan path is headlessly testable over a confined file.
            if json["source"] == nil {
                return .scanDocument(source: .camera)
            }
            guard let src = decodeSource(json: json) else { return .unknown(raw: raw) }
            return .scanDocument(source: src)
        case "screen.context.start":
            // Source is OPTIONAL: absent -> .screen (the continuous screen-context
            // default). An explicit source (incl. .file) is accepted so the loop is
            // headlessly testable over a confined file, like read.screen.
            // `interval_secs` is OPTIONAL: absent/invalid -> 30s (the daemon's
            // [screen_context].interval_secs default), floored at use.
            let rawInterval = (json["interval_secs"] as? Double)
                ?? (json["interval_secs"] as? NSNumber)?.doubleValue
                ?? 30
            // Clamp to a sane, FINITE range: an unbounded / NaN / infinite
            // interval_secs would trap the Double->UInt64 nanoseconds conversion in
            // the sleep and crash the app. 0…86_400s (one day); non-finite -> 30s.
            let interval = rawInterval.isFinite ? min(max(rawInterval, 0), 86_400) : 30
            if json["source"] == nil {
                return .screenContextStart(source: .screen, intervalSecs: interval)
            }
            guard let src = decodeSource(json: json) else { return .unknown(raw: raw) }
            return .screenContextStart(source: src, intervalSecs: interval)
        case "screen.context.stop":
            return .screenContextStop
        default:
            return .unknown(raw: raw)
        }
    }

    /// Decode a CaptureSource from an op body. Shape:
    ///   {"source":"camera"} | {"source":"screen"} | {"source":"file","path":"..."}
    private static func decodeSource(json: [String: Any]) -> CaptureSource? {
        guard let s = json["source"] as? String else { return nil }
        switch s {
        case "camera": return .camera
        case "screen": return .screen
        case "file":
            guard let path = json["path"] as? String, !path.isEmpty else { return nil }
            return .file(path: path)
        default:
            return nil
        }
    }
}
