// SharedTypesTests.swift — pure-logic tests for the FROZEN shared types and the
// app->host / host->app wire contract. NO camera, NO screen, NO TCC, NO socket.

import XCTest
import Foundation
@testable import vision

final class OpDecodeTests: XCTestCase {

    func testControlVerbs() {
        XCTAssertEqual(Op.decode(line: #"{"type":"start"}"#), .start)
        XCTAssertEqual(Op.decode(line: #"{"type":"refresh"}"#), .refresh)
        XCTAssertEqual(Op.decode(line: #"{"type":"stop"}"#), .stop)
    }

    func testWatchStartSources() {
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"watch.start","source":"camera"}"#),
            .watchStart(source: .camera))
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"watch.start","source":"screen"}"#),
            .watchStart(source: .screen))
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"watch.start","source":"file","path":"videos/input/x.mov"}"#),
            .watchStart(source: .file(path: "videos/input/x.mov")))
    }

    func testWatchStartFileWithoutPathIsUnknown() {
        if case .unknown = Op.decode(line: #"{"type":"op","op":"watch.start","source":"file"}"#) {
            // expected
        } else {
            XCTFail("file source without path must decode to .unknown")
        }
    }

    func testAnalyzeFileAndStopAndSensitivity() {
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"analyze.file","path":"videos/input/a.mp4"}"#),
            .analyzeFile(path: "videos/input/a.mp4"))
        XCTAssertEqual(Op.decode(line: #"{"type":"op","op":"watch.stop"}"#), .watchStop)
        XCTAssertEqual(Op.decode(line: #"{"type":"op","op":"status"}"#), .status)
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"set.sensitivity","value":0.75}"#),
            .setSensitivity(value: 0.75))
    }

    func testMalformedAndUnknownAreTotal() {
        if case .unknown = Op.decode(line: "not json") {} else { XCTFail("garbage -> unknown") }
        if case .unknown = Op.decode(line: "") {} else { XCTFail("empty -> unknown") }
        if case .unknown = Op.decode(line: #"{"type":"op","op":"bogus"}"#) {} else {
            XCTFail("unknown op -> unknown")
        }
        if case .unknown = Op.decode(line: #"{"type":"op","op":"set.sensitivity"}"#) {} else {
            XCTFail("set.sensitivity without value -> unknown")
        }
    }

    // --- read.screen (the additive OCR screen-read op) ----------------------

    func testReadScreenDefaultsToScreenWhenNoSource() {
        // Bare read.screen (no source) -> .readScreen(.screen): the "read my
        // screen" default.
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"read.screen"}"#),
            .readScreen(source: .screen))
    }

    func testReadScreenAcceptsExplicitSource() {
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"read.screen","source":"screen"}"#),
            .readScreen(source: .screen))
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"read.screen","source":"camera"}"#),
            .readScreen(source: .camera))
        // Explicit .file source is accepted (the headlessly-testable read path).
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"read.screen","source":"file","path":"videos/input/ui.png"}"#),
            .readScreen(source: .file(path: "videos/input/ui.png")))
    }

    func testReadScreenFileWithoutPathIsUnknown() {
        if case .unknown = Op.decode(line: #"{"type":"op","op":"read.screen","source":"file"}"#) {
            // expected — a file source needs a path.
        } else {
            XCTFail("read.screen file source without path must decode to .unknown")
        }
    }

    func testReadScreenWireName() {
        XCTAssertEqual(Op.readScreen(source: .screen).wireName, "read.screen")
    }

    // --- describe.capture (the additive VLM screen-capture op) --------------

    func testDescribeCaptureDefaultsToScreenWhenNoSource() {
        // describe.capture with a path but no source -> .describeCapture(path,
        // .screen): the "describe my screen" capture default. The path is the
        // confined PNG the app must write for the host's VLM.
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"describe.capture","path":"state/vision/f.png"}"#),
            .describeCapture(path: "state/vision/f.png", source: .screen))
    }

    func testDescribeCaptureAcceptsExplicitSource() {
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"describe.capture","path":"f.png","source":"screen"}"#),
            .describeCapture(path: "f.png", source: .screen))
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"describe.capture","path":"f.png","source":"camera"}"#),
            .describeCapture(path: "f.png", source: .camera))
        // Explicit .file source is accepted (the headlessly-testable capture path):
        // the op-level `path` (write target) and the file source `path` share one
        // key here, so both resolve to "out.png".
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"describe.capture","path":"out.png","source":"file"}"#),
            .describeCapture(path: "out.png", source: .file(path: "out.png")))
    }

    func testDescribeCaptureWithoutPathIsUnknown() {
        // describe.capture WITHOUT a path is malformed — the app refuses to capture
        // without a write target, so it decodes to .unknown (dropped + reported).
        if case .unknown = Op.decode(line: #"{"type":"op","op":"describe.capture"}"#) {
            // expected.
        } else {
            XCTFail("describe.capture without a path must decode to .unknown")
        }
        if case .unknown = Op.decode(line: #"{"type":"op","op":"describe.capture","source":"screen"}"#) {
            // expected — source but no path is still malformed.
        } else {
            XCTFail("describe.capture with a source but no path must decode to .unknown")
        }
    }

    func testDescribeCaptureWireName() {
        XCTAssertEqual(
            Op.describeCapture(path: "f.png", source: .screen).wireName, "describe.capture")
    }

    // --- classify.sound (the additive Sound Analysis op) --------------------

    func testClassifySoundDecodesWithPath() {
        // classify.sound with a path -> .classifySound(path): the "what was that
        // sound" op over a host-supplied audio clip.
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"classify.sound","path":"state/vision/clip.wav"}"#),
            .classifySound(path: "state/vision/clip.wav"))
    }

    func testClassifySoundWithoutPathIsUnknown() {
        // classify.sound WITHOUT a path is malformed — the app refuses to classify
        // without a clip (it never opens the mic), so it decodes to .unknown.
        if case .unknown = Op.decode(line: #"{"type":"op","op":"classify.sound"}"#) {
            // expected.
        } else {
            XCTFail("classify.sound without a path must decode to .unknown")
        }
        if case .unknown = Op.decode(line: #"{"type":"op","op":"classify.sound","path":""}"#) {
            // expected — an empty path is still malformed.
        } else {
            XCTFail("classify.sound with an empty path must decode to .unknown")
        }
    }

    func testClassifySoundWireName() {
        XCTAssertEqual(Op.classifySound(path: "clip.wav").wireName, "classify.sound")
    }

    // --- read.handwriting (#28, additive handwriting/whiteboard op) ---------

    func testReadHandwritingDefaultsToCameraWhenNoSource() {
        // read.handwriting with no source -> .readHandwriting(.camera): a
        // handwriting/whiteboard read off the camera is the default.
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"read.handwriting"}"#),
            .readHandwriting(source: .camera))
    }

    func testReadHandwritingAcceptsExplicitSource() {
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"read.handwriting","source":"screen"}"#),
            .readHandwriting(source: .screen))
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"read.handwriting","source":"camera"}"#),
            .readHandwriting(source: .camera))
        // Explicit .file source is accepted (the headlessly-testable read path).
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"read.handwriting","source":"file","path":"note.png"}"#),
            .readHandwriting(source: .file(path: "note.png")))
    }

    func testReadHandwritingFileWithoutPathIsUnknown() {
        if case .unknown = Op.decode(line: #"{"type":"op","op":"read.handwriting","source":"file"}"#) {
            // expected — a file source needs a path.
        } else {
            XCTFail("read.handwriting file source without path must decode to .unknown")
        }
    }

    func testReadHandwritingWireName() {
        XCTAssertEqual(Op.readHandwriting(source: .camera).wireName, "read.handwriting")
    }

    // --- scan.document (#29, additive camera document-scanner op) -----------

    func testScanDocumentDefaultsToCameraWhenNoSource() {
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"scan.document"}"#),
            .scanDocument(source: .camera))
    }

    func testScanDocumentAcceptsExplicitSource() {
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"scan.document","source":"camera"}"#),
            .scanDocument(source: .camera))
        // Explicit .file source is accepted (the headlessly-testable scan path).
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"scan.document","source":"file","path":"doc.png"}"#),
            .scanDocument(source: .file(path: "doc.png")))
    }

    func testScanDocumentFileWithoutPathIsUnknown() {
        if case .unknown = Op.decode(line: #"{"type":"op","op":"scan.document","source":"file"}"#) {
            // expected — a file source needs a path.
        } else {
            XCTFail("scan.document file source without path must decode to .unknown")
        }
    }

    func testScanDocumentWireName() {
        XCTAssertEqual(Op.scanDocument(source: .camera).wireName, "scan.document")
    }

    // --- screen.context.start/stop (#42, additive continuous-loop control) ---

    func testScreenContextStartDefaultsToScreenAndThirtySeconds() {
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"screen.context.start"}"#),
            .screenContextStart(source: .screen, intervalSecs: 30))
    }

    func testScreenContextStartAcceptsSourceAndInterval() {
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"screen.context.start","source":"screen","interval_secs":15}"#),
            .screenContextStart(source: .screen, intervalSecs: 15))
        // Explicit .file source is accepted (the headlessly-testable loop path).
        XCTAssertEqual(
            Op.decode(line: #"{"type":"op","op":"screen.context.start","source":"file","path":"ui.png","interval_secs":5}"#),
            .screenContextStart(source: .file(path: "ui.png"), intervalSecs: 5))
    }

    func testScreenContextStartFileWithoutPathIsUnknown() {
        if case .unknown = Op.decode(line: #"{"type":"op","op":"screen.context.start","source":"file"}"#) {
            // expected — a file source needs a path.
        } else {
            XCTFail("screen.context.start file source without path must decode to .unknown")
        }
    }

    func testScreenContextStopDecodes() {
        XCTAssertEqual(Op.decode(line: #"{"type":"op","op":"screen.context.stop"}"#), .screenContextStop)
    }

    func testScreenContextWireNames() {
        XCTAssertEqual(Op.screenContextStart(source: .screen, intervalSecs: 30).wireName,
                       "screen.context.start")
        XCTAssertEqual(Op.screenContextStop.wireName, "screen.context.stop")
    }

    func testFrozenOpWireNamesUnchanged() {
        // The FROZEN op shapes must keep their exact wire names (read.screen is
        // purely additive — it must not perturb the existing vocabulary).
        XCTAssertEqual(Op.start.wireName, "start")
        XCTAssertEqual(Op.refresh.wireName, "refresh")
        XCTAssertEqual(Op.stop.wireName, "stop")
        XCTAssertEqual(Op.watchStart(source: .camera).wireName, "watch.start")
        XCTAssertEqual(Op.watchStop.wireName, "watch.stop")
        XCTAssertEqual(Op.analyzeFile(path: "x").wireName, "analyze.file")
        XCTAssertEqual(Op.setSensitivity(value: 0.5).wireName, "set.sensitivity")
        XCTAssertEqual(Op.status.wireName, "status")
        XCTAssertNil(Op.unknown(raw: "x").wireName)
    }
}

final class VisionEventWireTests: XCTestCase {

    /// Decode the JSONL line an event produces back into a dictionary.
    private func decodeLine(_ line: String) -> [String: Any]? {
        guard let data = line.data(using: .utf8),
              let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        else { return nil }
        return obj
    }

    func testEnvelopeShapeAndTokenStamp() throws {
        let ev = VisionEvent.detections(
            frameIndex: 7, timestamp: 1.5, source: "file",
            detections: [Detection(kind: .human, boundingBox: .full, confidence: 0.9)])
        let line = try XCTUnwrap(ev.line(token: "TOKEN123"))
        let obj = try XCTUnwrap(decodeLine(line))
        XCTAssertEqual(obj["token"] as? String, "TOKEN123")
        XCTAssertEqual(obj["type"] as? String, "items")  // detections -> items
        let data = try XCTUnwrap(obj["data"] as? [String: Any])
        XCTAssertEqual(data["topic"] as? String, "vision.detections")
        XCTAssertEqual(data["count"] as? Int, 1)
        XCTAssertEqual(data["frame"] as? Int, 7)
        let byKind = try XCTUnwrap(data["by_kind"] as? [String: Int])
        XCTAssertEqual(byKind["human"], 1)
    }

    func testStatusIsRelayTypeStatusWithTopic() throws {
        let ev = VisionEvent.status(state: .watching, source: "camera", sensitivity: 0.5,
                                    cameraAuthorized: false, screenAuthorized: nil, message: "tcc pending")
        let obj = try XCTUnwrap(decodeLine(try XCTUnwrap(ev.line(token: "T"))))
        XCTAssertEqual(obj["type"] as? String, "status")
        let data = try XCTUnwrap(obj["data"] as? [String: Any])
        XCTAssertEqual(data["topic"] as? String, "vision.status")
        XCTAssertEqual(data["state"] as? String, "watching")
        XCTAssertEqual(data["source"] as? String, "camera")
        XCTAssertEqual(data["camera_authorized"] as? Bool, false)
        XCTAssertNil(data["screen_authorized"])  // nil -> omitted
    }

    func testModulesEnvelopeShapeIsTypedModulesWithNoTopic() throws {
        let ev = VisionEvent.modules([
            DyldModule(path: "/usr/lib/libSystem.B.dylib", uuid: "AAAA"),
            DyldModule(path: "/app/vision", uuid: nil),
        ])
        // Additive contract: type is "modules" (not items/status), NOT topic-routed.
        XCTAssertEqual(ev.relayType, .modules)
        let obj = try XCTUnwrap(decodeLine(try XCTUnwrap(ev.line(token: "T"))))
        XCTAssertEqual(obj["type"] as? String, "modules")
        let data = try XCTUnwrap(obj["data"] as? [String: Any])
        XCTAssertNil(data["topic"], "a modules report is not topic-routed")
        let mods = try XCTUnwrap(data["modules"] as? [[String: Any]])
        XCTAssertEqual(mods.count, 2)
        XCTAssertEqual(mods[0]["path"] as? String, "/usr/lib/libSystem.B.dylib")
        XCTAssertEqual(mods[0]["uuid"] as? String, "AAAA")
        XCTAssertEqual(mods[1]["path"] as? String, "/app/vision")
        XCTAssertNil(mods[1]["uuid"], "a nil uuid is omitted, not serialized as null")
    }

    func testCollectLoadedModulesReturnsThisProcessImages() throws {
        // Runs in-process (the test binary) — must enumerate real images with UUIDs.
        let mods = DyldReport.collectLoadedModules()
        XCTAssertFalse(mods.isEmpty, "the test process has loaded images")
        XCTAssertTrue(mods.allSatisfy { !$0.path.isEmpty })
        XCTAssertTrue(mods.contains { $0.uuid != nil }, "at least one image has a parsed LC_UUID")
    }

    func testWatchFiresOnARuntimeDlopen() throws {
        DyldReport.watch()
        // Registration clears the initial bulk, so nothing has changed yet.
        XCTAssertFalse(DyldReport.consumeChanged(), "no dlopen since watch() -> no change")
        let before = DyldReport.collectLoadedModules().count
        // dlopen a framework unlikely to be already loaded in the test binary.
        for lib in ["/System/Library/Frameworks/NaturalLanguage.framework/NaturalLanguage",
                    "/System/Library/Frameworks/Vision.framework/Vision",
                    "/usr/lib/libcups.2.dylib"] {
            if dlopen(lib, RTLD_NOW) != nil { break }
        }
        let after = DyldReport.collectLoadedModules().count
        // If the dlopen actually added images, the watch flag must have fired.
        if after > before {
            XCTAssertTrue(DyldReport.consumeChanged(), "a runtime dlopen must set the changed flag")
            XCTAssertFalse(DyldReport.consumeChanged(), "consumeChanged clears the flag")
        }
    }

    func testEveryTopicMatchesManifestDeclaredTopics() throws {
        // The relay drops any topic not declared in manifest.toml. These MUST
        // equal the manifest's telemetry_topics exactly (vision.screen for the OCR
        // readout; vision.sound for the Sound Analysis class readout — LABELS only).
        XCTAssertEqual(
            Set(VisionTopic.all),
            Set(["vision.detections", "vision.status", "vision.motion", "vision.perf",
                 "vision.error", "vision.screen", "vision.sound"]))
    }

    func testPerfMotionErrorTopics() throws {
        XCTAssertEqual(VisionEvent.perf(p50Ms: 1, p95Ms: 2, fps: 30, frames: 10, computeUnit: "ane").topic,
                       "vision.perf")
        XCTAssertEqual(VisionEvent.motion(frameIndex: 1, timestamp: 0, source: "screen",
                                          magnitude: 0.3, region: .full).topic, "vision.motion")
        XCTAssertEqual(VisionEvent.error(code: "tcc_denied", message: "x", source: "camera").topic,
                       "vision.error")
    }

    /// The vision.screen OCR readout encodes as type:"items" on the vision.screen
    /// topic and carries the full text, per-block string+box+center+confidence+
    /// is_control, the controls subset, and (when a query was given) the located
    /// block with a score. This is the shape the wiring stage consumes.
    func testScreenEventShape() throws {
        let blocks = [
            Detection(kind: .text, boundingBox: DetectionBox(x: 0.1, y: 0.8, width: 0.3, height: 0.06),
                      confidence: 0.97, label: "Settings"),
            Detection(kind: .text, boundingBox: DetectionBox(x: 0.1, y: 0.1, width: 0.2, height: 0.06),
                      confidence: 0.95, label: "Submit"),
        ]
        let readout = ScreenStructurer.structure(blocks)
        let located = ScreenStructurer.locate("submit", in: readout)
        let ev = VisionEvent.screen(frameIndex: 3, timestamp: 1.25, source: "screen",
                                    readout: readout, located: located, query: "submit",
                                    meta: .screen)
        XCTAssertEqual(ev.topic, "vision.screen")
        XCTAssertEqual(ev.relayType, .items)

        let obj = try XCTUnwrap(decodeLine(try XCTUnwrap(ev.line(token: "T"))))
        XCTAssertEqual(obj["type"] as? String, "items")
        let data = try XCTUnwrap(obj["data"] as? [String: Any])
        XCTAssertEqual(data["topic"] as? String, "vision.screen")
        XCTAssertEqual(data["frame"] as? Int, 3)
        XCTAssertEqual(data["source"] as? String, "screen")
        XCTAssertEqual(data["block_count"] as? Int, 2)
        // Reading order: "Settings" (high y) before "Submit" (low y).
        let fullText = try XCTUnwrap(data["text"] as? String)
        XCTAssertEqual(fullText, "Settings\nSubmit")
        let arr = try XCTUnwrap(data["blocks"] as? [[String: Any]])
        XCTAssertEqual(arr.count, 2)
        let first = arr[0]
        XCTAssertEqual(first["text"] as? String, "Settings")
        XCTAssertEqual(first["is_control"] as? Bool, true)
        let box = try XCTUnwrap(first["box"] as? [String: Any])
        XCTAssertNotNil(box["x"]); XCTAssertNotNil(box["w"])
        let center = try XCTUnwrap(first["center"] as? [String: Any])
        XCTAssertNotNil(center["x"]); XCTAssertNotNil(center["y"])
        // controls subset present.
        let controls = try XCTUnwrap(data["controls"] as? [[String: Any]])
        XCTAssertEqual(controls.count, 2, "both short labels are control candidates")
        // located block for the query.
        XCTAssertEqual(data["query"] as? String, "submit")
        let loc = try XCTUnwrap(data["located"] as? [String: Any])
        XCTAssertEqual(loc["text"] as? String, "Submit")
        XCTAssertNotNil(loc["score"])
    }

    func testScreenEventOmitsLocatedWhenNoQuery() throws {
        let readout = ScreenStructurer.structure(
            [Detection(kind: .text, boundingBox: .full, confidence: 0.9, label: "hi")])
        let ev = VisionEvent.screen(frameIndex: 0, timestamp: 0, source: "file",
                                    readout: readout, located: nil, query: nil, meta: .screen)
        let obj = try XCTUnwrap(decodeLine(try XCTUnwrap(ev.line(token: "T"))))
        let data = try XCTUnwrap(obj["data"] as? [String: Any])
        XCTAssertNil(data["located"], "no query -> no located block")
        XCTAssertNil(data["query"])
        // The plain screen read tags read_kind=screen and omits document_detected.
        XCTAssertEqual(data["read_kind"] as? String, "screen")
        XCTAssertNil(data["document_detected"], "screen OCR has no document-detected bool")
    }

    /// #28 + #29 telemetry: the vision.screen readout carries the NON-RAW-TEXT
    /// signal — read_kind, text_present + text_length (presence + length, NOT the
    /// glyphs), and the HONEST document_detected bool for the scanner. This is the
    /// safe-to-summarize status surface; the raw text rides `text`/`blocks`
    /// (transient). Asserts both the handwriting (#28) and the document (#29)
    /// readouts, including a NO-document scan (document_detected=false, empty).
    func testHandwritingAndDocumentReadoutsCarryPresenceLengthAndDocBool() throws {
        // #28 handwriting: read_kind=handwriting, text present, length matches,
        // NO document_detected (not applicable to a handwriting read).
        let hwReadout = ScreenStructurer.structure([
            Detection(kind: .text, boundingBox: DetectionBox(x: 0.1, y: 0.7, width: 0.4, height: 0.08),
                      confidence: 0.9, label: "Buy milk"),
        ])
        let hwEv = VisionEvent.screen(frameIndex: 1, timestamp: 0, source: "camera",
                                      readout: hwReadout, located: nil, query: nil,
                                      meta: ScreenReadMeta(kind: .handwriting))
        let hwData = try XCTUnwrap(decodeLine(try XCTUnwrap(hwEv.line(token: "T"))))["data"] as? [String: Any]
        XCTAssertEqual(hwData?["read_kind"] as? String, "handwriting")
        XCTAssertEqual(hwData?["text_present"] as? Bool, true)
        XCTAssertEqual(hwData?["text_length"] as? Int, "Buy milk".count)
        XCTAssertNil(hwData?["document_detected"], "handwriting has no document-detected bool")

        // #29 document (page found): read_kind=document, document_detected=true,
        // text present.
        let docReadout = ScreenStructurer.structure([
            Detection(kind: .text, boundingBox: DetectionBox(x: 0.1, y: 0.8, width: 0.3, height: 0.06),
                      confidence: 0.95, label: "INVOICE"),
        ])
        let docEv = VisionEvent.screen(frameIndex: 2, timestamp: 0, source: "camera",
                                       readout: docReadout, located: nil, query: nil,
                                       meta: ScreenReadMeta(kind: .document, documentDetected: true))
        let docData = try XCTUnwrap(decodeLine(try XCTUnwrap(docEv.line(token: "T"))))["data"] as? [String: Any]
        XCTAssertEqual(docData?["read_kind"] as? String, "document")
        XCTAssertEqual(docData?["document_detected"] as? Bool, true)
        XCTAssertEqual(docData?["text_present"] as? Bool, true)
        XCTAssertEqual(docData?["text_length"] as? Int, "INVOICE".count)

        // #29 NO document: document_detected=false, text absent, length 0 — the
        // honest empty (never a fabricated page).
        let emptyEv = VisionEvent.screen(frameIndex: 3, timestamp: 0, source: "camera",
                                         readout: ScreenStructurer.structure([]),
                                         located: nil, query: nil,
                                         meta: ScreenReadMeta(kind: .document, documentDetected: false))
        let emptyData = try XCTUnwrap(decodeLine(try XCTUnwrap(emptyEv.line(token: "T"))))["data"] as? [String: Any]
        XCTAssertEqual(emptyData?["document_detected"] as? Bool, false)
        XCTAssertEqual(emptyData?["text_present"] as? Bool, false)
        XCTAssertEqual(emptyData?["text_length"] as? Int, 0)
        XCTAssertEqual(emptyData?["block_count"] as? Int, 0)
    }

    /// The vision.sound class readout encodes as type:"items" on the vision.sound
    /// topic and carries the top sound classes (label + confidence ONLY), the
    /// classifier tag (the fixed ~300-class version1), and the compute_unit tag.
    /// CRUCIAL PRIVACY ASSERTION: there is NO audio field anywhere in the payload —
    /// only the derived LABELS cross the socket. This is the shape the daemon's
    /// identify-sound intent + ambient monitor consume.
    func testSoundEventShapeAndNoAudioLeaks() throws {
        let classes = [
            SoundClass(label: "dog_bark", confidence: 0.82),
            SoundClass(label: "doorbell", confidence: 0.41),
        ]
        let ev = VisionEvent.sound(timestamp: 2.5, source: "sound", classes: classes,
                                   classifier: SoundEngine.classifierTag,
                                   computeUnit: SoundEngine.computeUnitTag)
        XCTAssertEqual(ev.topic, "vision.sound")
        XCTAssertEqual(ev.relayType, .items)

        let line = try XCTUnwrap(ev.line(token: "TKN"))
        let obj = try XCTUnwrap(decodeLine(line))
        XCTAssertEqual(obj["token"] as? String, "TKN")
        XCTAssertEqual(obj["type"] as? String, "items")
        let data = try XCTUnwrap(obj["data"] as? [String: Any])
        XCTAssertEqual(data["topic"] as? String, "vision.sound")
        XCTAssertEqual(data["source"] as? String, "sound")
        XCTAssertEqual(data["count"] as? Int, 2)
        XCTAssertEqual(data["classifier"] as? String, "SNClassifierIdentifier.version1")
        XCTAssertEqual(data["compute_unit"] as? String, "all")
        let arr = try XCTUnwrap(data["classes"] as? [[String: Any]])
        XCTAssertEqual(arr.count, 2)
        XCTAssertEqual(arr[0]["label"] as? String, "dog_bark")
        XCTAssertEqual(arr[0]["confidence"] as? Double, 0.82)
        // Each class entry carries ONLY label + confidence — nothing else.
        for entry in arr {
            XCTAssertEqual(Set(entry.keys), Set(["label", "confidence"]),
                           "a sound class must carry ONLY label + confidence")
        }
        // PRIVACY: the WHOLE serialized line must NOT contain any audio/sample/pcm
        // field — only derived labels cross the socket; the audio never leaves.
        let lowered = line.lowercased()
        for forbidden in ["\"audio\"", "\"pcm\"", "\"samples\"", "\"waveform\"", "\"buffer\""] {
            XCTAssertFalse(lowered.contains(forbidden),
                           "vision.sound must NEVER carry an audio field (found \(forbidden))")
        }
    }

    func testSoundEventTopicConstant() {
        XCTAssertEqual(VisionTopic.sound, "vision.sound")
    }
}

final class AppEnvTests: XCTestCase {

    func testLoadsRequiredKeys() throws {
        let env = try AppEnv.load(from: [
            "JARVIS_APP_TOKEN": "abc",
            "JARVIS_APP_SOCKET": "/tmp/state/ipc/apps/vision.sock",
            "JARVIS_APP_NAME": "vision",
        ])
        XCTAssertEqual(env.token, "abc")
        XCTAssertEqual(env.socketPath, "/tmp/state/ipc/apps/vision.sock")
        XCTAssertEqual(env.name, "vision")
        XCTAssertFalse(env.cameraDeclared)
        XCTAssertFalse(env.screenDeclared)
    }

    func testMissingTokenThrows() {
        XCTAssertThrowsError(try AppEnv.load(from: [
            "JARVIS_APP_SOCKET": "/tmp/x.sock", "JARVIS_APP_NAME": "vision",
        ])) { error in
            XCTAssertEqual(error as? AppEnv.EnvError, .missing("JARVIS_APP_TOKEN"))
        }
    }

    func testInterimCapabilityFlagsParse() throws {
        let env = try AppEnv.load(from: [
            "JARVIS_APP_TOKEN": "t", "JARVIS_APP_SOCKET": "/s", "JARVIS_APP_NAME": "vision",
            "JARVIS_VISION_CAMERA": "true", "JARVIS_VISION_SCREEN": "0",
        ])
        XCTAssertTrue(env.cameraDeclared)
        XCTAssertFalse(env.screenDeclared)
    }
}

final class DetectionTypeTests: XCTestCase {

    func testDetectionCodableRoundTrip() throws {
        let det = Detection(kind: .object, boundingBox: DetectionBox(x: 0.1, y: 0.2, width: 0.3, height: 0.4),
                            confidence: 0.42, label: "keyboard")
        let data = try JSONEncoder().encode(det)
        let back = try JSONDecoder().decode(Detection.self, from: data)
        XCTAssertEqual(det, back)
    }

    func testBoxCGRectConversion() {
        let r = CGRect(x: 0.1, y: 0.2, width: 0.3, height: 0.4)
        let b = DetectionBox(cgRect: r)
        XCTAssertEqual(b.cgRect, r)
    }

    func testCaptureSourceTags() {
        XCTAssertEqual(CaptureSource.camera.tag, "camera")
        XCTAssertEqual(CaptureSource.screen.tag, "screen")
        XCTAssertEqual(CaptureSource.file(path: "x").tag, "file")
    }
}

/// Proves the OutboundSink stamps the token + frames exactly one line per event
/// through an in-memory writer — the app->host wire adapter, with no socket.
final class OutboundSinkTests: XCTestCase {

    actor CollectingWriter: LineWriter {
        private(set) var lines: [String] = []
        func writeLine(_ line: String) async { lines.append(line) }
        func snapshot() -> [String] { lines }
    }

    func testSinkStampsTokenPerEvent() async throws {
        let writer = CollectingWriter()
        let sink = OutboundSink(token: "TKN", writer: writer)
        await sink.emit(.status(state: .idle, source: nil, sensitivity: 0.5,
                                cameraAuthorized: nil, screenAuthorized: nil, message: nil))
        await sink.emit(.motion(frameIndex: 1, timestamp: 0, source: "file",
                                magnitude: 0.5, region: .full))
        let lines = await writer.snapshot()
        XCTAssertEqual(lines.count, 2)
        for line in lines {
            let obj = try XCTUnwrap((try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? [String: Any])
            XCTAssertEqual(obj["token"] as? String, "TKN")
        }
    }
}
