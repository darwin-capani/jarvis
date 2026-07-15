// IPCTests.swift — tests for the IPC + main module (ipc_main agent).
//
// Covers: op parsing/dispatch over the REAL socket transport (socketpair, no
// daemon), token-on-EVERY-line via the real SocketWriter, unknown op =
// clean error not crash, and analyze.file / watch.start(file:) path confinement
// to the granted videos/input dir (lexical + real-path, including the
// symlink-escape case). NO camera, NO screen, NO TCC, NO daemon-bound socket.

import XCTest
import Foundation
#if canImport(Darwin)
import Darwin
#endif
@testable import vision

// ===========================================================================
// Path confinement — the security-critical surface for file-bearing ops.
// ===========================================================================

final class VideoPathConfinementTests: XCTestCase {

    /// A temp project root with apps/vision/videos/input materialized + one real
    /// video-named file inside it. Returns (root, resolver, realInputDir).
    private func makeTempProject() throws -> (root: String, resolver: VideoPathResolver, inputDir: String) {
        let base = NSTemporaryDirectory() + "vision-ipc-\(UUID().uuidString)"
        let inputDir = (base as NSString).appendingPathComponent("apps/vision/videos/input")
        try FileManager.default.createDirectory(atPath: inputDir, withIntermediateDirectories: true)
        return (base, VideoPathResolver(projectRoot: base), inputDir)
    }

    func testGrantedDirIsAppsVisionVideosInput() {
        let r = VideoPathResolver(projectRoot: "/Users/x/darwin")
        XCTAssertEqual(r.grantedDir, "/Users/x/darwin/apps/vision/videos/input")
    }

    func testLexicalRejectsAbsolutePath() {
        let r = VideoPathResolver(projectRoot: "/root")
        XCTAssertThrowsError(try r.resolveLexical("/etc/passwd")) { e in
            XCTAssertEqual(e as? VisionPathError, .notPermitted("/etc/passwd"))
        }
    }

    func testLexicalRejectsParentTraversal() {
        let r = VideoPathResolver(projectRoot: "/root")
        for bad in ["../secret.mov", "videos/input/../../../secret.mov", "a/../../b.mov"] {
            XCTAssertThrowsError(try r.resolveLexical(bad), "must reject \(bad)") { e in
                XCTAssertEqual((e as? VisionPathError)?.code, "path_denied", "for \(bad)")
            }
        }
    }

    func testLexicalAcceptsBareNameAndRootedPath() throws {
        let r = VideoPathResolver(projectRoot: "/root")
        // Bare filename -> placed directly under the granted dir.
        XCTAssertEqual(try r.resolveLexical("clip.mov"),
                       "/root/apps/vision/videos/input/clip.mov")
        // videos/input/-rooted -> same place (tail normalized).
        XCTAssertEqual(try r.resolveLexical("videos/input/clip.mp4"),
                       "/root/apps/vision/videos/input/clip.mp4")
        // apps/vision/videos/input/-rooted -> same place.
        XCTAssertEqual(try r.resolveLexical("apps/vision/videos/input/clip.m4v"),
                       "/root/apps/vision/videos/input/clip.m4v")
        // A subdir under the granted dir is allowed (no traversal).
        XCTAssertEqual(try r.resolveLexical("sub/clip.mov"),
                       "/root/apps/vision/videos/input/sub/clip.mov")
    }

    func testLexicalRejectsUnsupportedExtension() {
        let r = VideoPathResolver(projectRoot: "/root")
        XCTAssertThrowsError(try r.resolveLexical("clip.txt")) { e in
            XCTAssertEqual((e as? VisionPathError)?.code, "unsupported_file")
        }
        // No extension at all -> unsupported.
        XCTAssertThrowsError(try r.resolveLexical("clip")) { e in
            XCTAssertEqual((e as? VisionPathError)?.code, "unsupported_file")
        }
    }

    func testRealConfineAcceptsFileInsideGrantedDir() throws {
        let p = try makeTempProject()
        let file = (p.inputDir as NSString).appendingPathComponent("ok.mov")
        FileManager.default.createFile(atPath: file, contents: Data([0, 1, 2, 3]))
        let resolved = try p.resolver.resolve("ok.mov")
        // realpath canonicalizes (macOS /var -> /private/var); both sides resolved.
        XCTAssertTrue(resolved.hasSuffix("/apps/vision/videos/input/ok.mov"))
    }

    func testRealConfineRejectsMissingFile() throws {
        let p = try makeTempProject()
        XCTAssertThrowsError(try p.resolver.resolve("nope.mov")) { e in
            XCTAssertEqual((e as? VisionPathError)?.code, "not_found")
        }
    }

    func testRealConfineRejectsSymlinkEscape() throws {
        let p = try makeTempProject()
        // Plant a secret OUTSIDE the granted dir, then a symlink INSIDE it that
        // points out. The symlink has no `..` and is not absolute, so it passes
        // the lexical pass — only the real-path guard catches it.
        let secretDir = (p.root as NSString).appendingPathComponent("secret")
        try FileManager.default.createDirectory(atPath: secretDir, withIntermediateDirectories: true)
        let secret = (secretDir as NSString).appendingPathComponent("escape.mov")
        FileManager.default.createFile(atPath: secret, contents: Data([9, 9, 9]))
        let link = (p.inputDir as NSString).appendingPathComponent("escape.mov")
        try FileManager.default.createSymbolicLink(atPath: link, withDestinationPath: secret)

        // Lexical pass accepts it (no traversal in the string).
        XCTAssertNoThrow(try p.resolver.resolveLexical("escape.mov"))
        // Full resolve REJECTS it (real target escapes the granted root).
        XCTAssertThrowsError(try p.resolver.resolve("escape.mov")) { e in
            XCTAssertEqual((e as? VisionPathError)?.code, "path_denied")
        }
    }

    func testConfineFileOpRewritesAnalyzeFileToCanonicalPath() async throws {
        let p = try makeTempProject()
        let file = (p.inputDir as NSString).appendingPathComponent("clip.mp4")
        FileManager.default.createFile(atPath: file, contents: Data([1, 2]))
        let sink = RecordingSink()
        let out = await VisionApp.confineFileOp(.analyzeFile(path: "clip.mp4"),
                                                resolver: p.resolver, sink: sink)
        guard case let .analyzeFile(rewritten)? = out else {
            return XCTFail("expected a rewritten analyzeFile op, got \(String(describing: out))")
        }
        XCTAssertTrue(rewritten.hasSuffix("/apps/vision/videos/input/clip.mp4"))
        // No error emitted on the happy path.
        let events = await sink.snapshot()
        XCTAssertTrue(events.isEmpty, "no error expected for a confined path")
    }

    func testConfineFileOpRejectsTraversalAndEmitsError() async throws {
        let p = try makeTempProject()
        let sink = RecordingSink()
        let out = await VisionApp.confineFileOp(.analyzeFile(path: "../../../etc/passwd"),
                                                resolver: p.resolver, sink: sink)
        XCTAssertNil(out, "a traversal op must be dropped (nil)")
        let events = await sink.snapshot()
        XCTAssertEqual(events.count, 1)
        guard case let .error(code, _, source) = events[0] else {
            return XCTFail("expected a vision.error, got \(events[0])")
        }
        XCTAssertEqual(code, "path_denied")
        XCTAssertEqual(source, "file")
    }

    func testConfineFileOpPassesNonFileOpsThrough() async {
        let p = VideoPathResolver(projectRoot: "/root")
        let sink = RecordingSink()
        for op: Op in [.status, .watchStop, .setSensitivity(value: 0.3), .watchStart(source: .camera)] {
            let out = await VisionApp.confineFileOp(op, resolver: p, sink: sink)
            XCTAssertEqual(out, op, "non-file op must pass through unchanged")
        }
        let events = await sink.snapshot()
        XCTAssertTrue(events.isEmpty)
    }
}

// ===========================================================================
// Real socket transport: token-on-every-line + read/dispatch loop.
// ===========================================================================
#if canImport(Darwin)

final class SocketTransportTests: XCTestCase {

    func testWriteThenReadLineRoundTrips() async throws {
        let (a, b) = try makeSocketConnectionPair()
        await a.writer.writeLine(#"{"type":"start"}"#)
        await a.writer.writeLine(#"{"type":"op","op":"status"}"#)
        let l1 = await b.reader.readLine()
        let l2 = await b.reader.readLine()
        XCTAssertEqual(l1, #"{"type":"start"}"#)
        XCTAssertEqual(l2, #"{"type":"op","op":"status"}"#)
        await a.reader.close(); await a.writer.close()
        await b.reader.close(); await b.writer.close()
    }

    func testReadLineReturnsNilAtEOF() async throws {
        let (a, b) = try makeSocketConnectionPair()
        await a.writer.writeLine("solo")
        await a.writer.close() // peer write half hangs up
        await a.reader.close()
        let firstLine = await b.reader.readLine()
        XCTAssertEqual(firstLine, "solo")
        let eofLine = await b.reader.readLine()
        XCTAssertNil(eofLine, "EOF after the last line must be nil")
        await b.reader.close(); await b.writer.close()
    }

    /// OutboundSink over the REAL SocketWriter stamps the token on EVERY line
    /// (the daemon verifies this per-line; an unstamped line is dropped).
    func testOutboundSinkOverSocketStampsTokenPerLine() async throws {
        let (appSide, hostSide) = try makeSocketConnectionPair()
        let sink = OutboundSink(token: "HEXTOKEN", writer: SocketLineWriter(writer: appSide.writer))

        await sink.emit(.status(state: .idle, source: nil, sensitivity: 0.5,
                                cameraAuthorized: nil, screenAuthorized: nil, message: nil))
        await sink.emit(.detections(frameIndex: 1, timestamp: 0, source: "file",
                                    detections: [Detection(kind: .human, boundingBox: .full, confidence: 0.9)]))
        await sink.emit(.perf(p50Ms: 1, p95Ms: 2, fps: 30, frames: 5, computeUnit: "all"))

        for _ in 0..<3 {
            let readLine = await hostSide.reader.readLine()
            let line = try XCTUnwrap(readLine)
            let obj = try XCTUnwrap(
                (try? JSONSerialization.jsonObject(with: Data(line.utf8))) as? [String: Any])
            XCTAssertEqual(obj["token"] as? String, "HEXTOKEN", "every line must carry the token")
            let type = obj["type"] as? String
            XCTAssertTrue(["items", "status", "log"].contains(type ?? ""), "type=\(type ?? "nil")")
        }
        await appSide.reader.close(); await appSide.writer.close()
        await hostSide.reader.close(); await hostSide.writer.close()
    }
}

final class SocketAppConnectionTests: XCTestCase {

    /// An EventSink/op collector for asserting what the connection dispatched.
    actor OpCollector {
        private(set) var ops: [Op] = []
        func record(_ op: Op) { ops.append(op) }
        func snapshot() -> [Op] { ops }
    }

    /// The connection decodes every host line via Op.decode and dispatches it,
    /// stopping cleanly after a {"type":"stop"} — and an UNKNOWN op decodes to
    /// .unknown (a clean value the pipeline turns into vision.error) rather than
    /// crashing the loop.
    func testRunDecodesDispatchesAndStops() async throws {
        let (appSide, hostSide) = try makeSocketConnectionPair()
        let collector = OpCollector()

        // Host queues: start, a valid op, the describe.capture op (proving it
        // decodes over the REAL transport, not just in the unit decoder), an
        // UNKNOWN op, garbage, then stop.
        await hostSide.writer.writeLine(#"{"type":"start"}"#)
        await hostSide.writer.writeLine(#"{"type":"op","op":"set.sensitivity","value":0.7}"#)
        await hostSide.writer.writeLine(#"{"type":"op","op":"describe.capture","path":"state/vision/f.png"}"#)
        await hostSide.writer.writeLine(#"{"type":"op","op":"format.disk","target":"/"}"#) // unknown op
        await hostSide.writer.writeLine("not even json")                                    // garbage
        await hostSide.writer.writeLine(#"{"type":"stop"}"#)

        let conn = SocketAppConnection(reader: appSide.reader)
        // run() returns after it processes the stop line (it reads until stop/EOF).
        try await conn.run { op in await collector.record(op) }

        let ops = await collector.snapshot()
        // start, setSensitivity(0.7), describeCapture, unknown(format.disk),
        // unknown(garbage), stop.
        XCTAssertEqual(ops.count, 6, "got \(ops)")
        XCTAssertEqual(ops[0], .start)
        XCTAssertEqual(ops[1], .setSensitivity(value: 0.7))
        XCTAssertEqual(ops[2], .describeCapture(path: "state/vision/f.png", source: .screen),
                       "describe.capture must decode over the real transport, not .unknown")
        if case .unknown = ops[3] {} else { XCTFail("unknown op must decode to .unknown, got \(ops[3])") }
        if case .unknown = ops[4] {} else { XCTFail("garbage must decode to .unknown, got \(ops[4])") }
        XCTAssertEqual(ops[5], .stop)
        await hostSide.reader.close(); await hostSide.writer.close()
        await appSide.writer.close()
    }

    /// The loop exits cleanly when the host closes the socket without a stop
    /// (EOF), never hanging or crashing.
    func testRunExitsOnEOFWithoutStop() async throws {
        let (appSide, hostSide) = try makeSocketConnectionPair()
        let collector = OpCollector()
        await hostSide.writer.writeLine(#"{"type":"op","op":"status"}"#)
        await hostSide.writer.close() // hang up the write half
        await hostSide.reader.close()

        let conn = SocketAppConnection(reader: appSide.reader)
        try await conn.run { op in await collector.record(op) }
        let ops = await collector.snapshot()
        XCTAssertEqual(ops, [.status])
        await appSide.writer.close()
    }

    /// END-TO-END over the real transport: a host op line in -> Pipeline handles
    /// it -> a token-stamped vision.status line back out to the host, proving the
    /// full ipc<->pipeline<->ipc wire path with no daemon and no camera.
    func testEndToEndOpInTelemetryOut() async throws {
        // Two independent connections: one carries host->app (the connection
        // reads), one carries app->host (the sink writes). In production these
        // are the same socket; here we split them so the test drives each side.
        let (appRead, hostWrite) = try makeSocketConnectionPair()   // host -> app
        let (appWrite, hostRead) = try makeSocketConnectionPair()   // app -> host

        let sink = OutboundSink(token: "T", writer: SocketLineWriter(writer: appWrite.writer))
        let pipeline = Pipeline(detector: StubDetector(), sink: sink)

        await hostWrite.writer.writeLine(#"{"type":"op","op":"watch.start","source":"camera"}"#)
        await hostWrite.writer.writeLine(#"{"type":"stop"}"#)

        let conn = SocketAppConnection(reader: appRead.reader)
        try await conn.run { op in await pipeline.handle(op) }

        // The Pipeline emits a vision.status for watch.start (watching). Read it
        // off the host side and assert the framing.
        let firstRead = await hostRead.reader.readLine()
        let first = try XCTUnwrap(firstRead)
        let obj = try XCTUnwrap((try? JSONSerialization.jsonObject(with: Data(first.utf8))) as? [String: Any])
        XCTAssertEqual(obj["token"] as? String, "T")
        XCTAssertEqual(obj["type"] as? String, "status")
        let data = try XCTUnwrap(obj["data"] as? [String: Any])
        XCTAssertEqual(data["topic"] as? String, "vision.status")
        XCTAssertEqual(data["state"] as? String, "watching")
        XCTAssertEqual(data["source"] as? String, "camera")

        await appRead.reader.close(); await appRead.writer.close()
        await appWrite.reader.close(); await appWrite.writer.close()
        await hostRead.reader.close(); await hostRead.writer.close()
        await hostWrite.reader.close(); await hostWrite.writer.close()
    }
}

#endif // canImport(Darwin)

// ===========================================================================
// Shared test sink — records emitted events for assertions (no socket).
// ===========================================================================

actor RecordingSink: EventSink {
    private(set) var events: [VisionEvent] = []
    func emit(_ event: VisionEvent) async { events.append(event) }
    func snapshot() -> [VisionEvent] { events }
}
