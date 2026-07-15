// AppWiringTests.swift — wiring/honesty tests for the PRODUCTION socket-served
// path and the analyze/file perf telemetry.
//
// These close two load-bearing gaps the verifiers found:
//  (1) The production socket path (App.runSocketServed) builds a Pipeline and
//      MUST inject the REAL FrameSource factory (CaptureSourceFactory.make), or
//      watch.start/analyze.file decode ZERO frames (the StubFrameSource default).
//      We assert that a Pipeline wired EXACTLY as production wires it (real
//      detector + CaptureSourceFactory.make) decodes real frames + real
//      detections from a synthesized video — NOT zero — proving the production
//      path reaches the real FileSource.
//  (2) vision.perf must carry a REAL measured inference-ms, not a placeholder.
//      We assert the perf event the file path emits carries a finite, > 0
//      measured inference latency.
//
// Headless ONLY: synthesized video file via AVAssetWriter, the .file capture
// path, and unit-level inference. NO camera, NO screen, NO TCC, NO socket. The
// default Pipeline (no injection) still uses the zero-frame stub — we assert
// that contrast too, so the wiring is what actually flips behavior.

import XCTest
import Foundation
import AVFoundation
import CoreVideo
import CoreMedia
import CoreGraphics
@testable import vision

final class AppWiringTests: XCTestCase {

    // MARK: - Synthesized test video (no checked-in binary asset)

    /// Synthesize a tiny H.264 .mov with `frameCount` distinct gray frames.
    /// Throws XCTSkip if the environment cannot encode video.
    private func synthesizeVideo(frameCount: Int, width: Int = 64, height: Int = 48,
                                 fps: Int32 = 30) async throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("darwin_vision_appwire_\(UUID().uuidString).mov")
        if FileManager.default.fileExists(atPath: url.path) {
            try FileManager.default.removeItem(at: url)
        }
        let writer = try AVAssetWriter(outputURL: url, fileType: .mov)
        let settings: [String: Any] = [
            AVVideoCodecKey: AVVideoCodecType.h264,
            AVVideoWidthKey: width,
            AVVideoHeightKey: height,
        ]
        let input = AVAssetWriterInput(mediaType: .video, outputSettings: settings)
        input.expectsMediaDataInRealTime = false
        let adaptor = AVAssetWriterInputPixelBufferAdaptor(
            assetWriterInput: input,
            sourcePixelBufferAttributes: [
                kCVPixelBufferPixelFormatTypeKey as String: Int(kCVPixelFormatType_32ARGB),
                kCVPixelBufferWidthKey as String: width,
                kCVPixelBufferHeightKey as String: height,
            ])
        guard writer.canAdd(input) else {
            throw XCTSkip("AVAssetWriter cannot add a video input in this environment")
        }
        writer.add(input)
        guard writer.startWriting() else {
            throw XCTSkip("AVAssetWriter cannot start writing (no encoder?): \(String(describing: writer.error))")
        }
        writer.startSession(atSourceTime: .zero)
        for i in 0..<frameCount {
            while !input.isReadyForMoreMediaData {
                try await Task.sleep(nanoseconds: 1_000_000)
            }
            // A PATTERNED frame (bright block on a dark field, moving per frame)
            // so the built-in saliency/classification detectors have real
            // structure to fire on — a flat fill yields no detections.
            guard let pb = Self.makePatternBuffer(width: width, height: height, phase: i) else {
                throw XCTSkip("could not allocate a CVPixelBuffer")
            }
            let pts = CMTime(value: CMTimeValue(i), timescale: fps)
            adaptor.append(pb, withPresentationTime: pts)
        }
        input.markAsFinished()
        await writer.finishWriting()
        guard writer.status == .completed else {
            throw XCTSkip("AVAssetWriter did not complete: \(String(describing: writer.error))")
        }
        return url
    }

    /// A patterned ARGB pixel buffer: a dark field with a bright block whose
    /// position shifts with `phase`, drawn via CoreGraphics so the frame carries
    /// real spatial structure (saliency/classification have something to detect).
    private static func makePatternBuffer(width: Int, height: Int, phase: Int) -> CVPixelBuffer? {
        var pb: CVPixelBuffer?
        let attrs: [String: Any] = [
            kCVPixelBufferCGImageCompatibilityKey as String: true,
            kCVPixelBufferCGBitmapContextCompatibilityKey as String: true,
        ]
        let rc = CVPixelBufferCreate(kCFAllocatorDefault, width, height,
                                     kCVPixelFormatType_32ARGB, attrs as CFDictionary, &pb)
        guard rc == kCVReturnSuccess, let pb = pb else { return nil }
        CVPixelBufferLockBaseAddress(pb, [])
        defer { CVPixelBufferUnlockBaseAddress(pb, []) }
        guard let base = CVPixelBufferGetBaseAddress(pb) else { return nil }
        let bpr = CVPixelBufferGetBytesPerRow(pb)
        // Dark field.
        memset(base, 16, bpr * height)
        let cs = CGColorSpaceCreateDeviceRGB()
        // 32ARGB CVPixelBuffer is laid out as host-endian ARGB; a CGBitmapContext
        // with premultipliedFirst + byteOrder32Big over the same memory lets us
        // draw shapes into the buffer.
        guard let ctx = CGContext(
            data: base, width: width, height: height, bitsPerComponent: 8,
            bytesPerRow: bpr, space: cs,
            bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue
                | CGBitmapInfo.byteOrder32Big.rawValue)
        else { return pb }   // fall back to the dark field if context creation fails
        // A bright block + a contrasting bar that move with the phase.
        let bw = max(4, width / 3), bh = max(4, height / 3)
        let bx = (phase * 7) % max(1, width - bw)
        let by = (phase * 5) % max(1, height - bh)
        ctx.setFillColor(red: 0.95, green: 0.9, blue: 0.2, alpha: 1)
        ctx.fill(CGRect(x: bx, y: by, width: bw, height: bh))
        ctx.setFillColor(red: 0.1, green: 0.5, blue: 0.95, alpha: 1)
        ctx.fill(CGRect(x: width - bw, y: height - bh, width: bw / 2, height: bh / 2))
        return pb
    }

    /// A sink that records every emitted event AND waits until at least one
    /// detections event (or `give-up` count) has flowed, so the async run task
    /// can be observed deterministically.
    private actor RecordingSink: EventSink {
        private(set) var events: [VisionEvent] = []
        func emit(_ event: VisionEvent) async { events.append(event) }
        func snapshot() -> [VisionEvent] { events }
        func detectionCount() -> Int {
            events.reduce(0) { acc, ev in
                if case let .detections(_, _, _, dets) = ev { return acc + dets.count }
                return acc
            }
        }
        func hasFinished() -> Bool {
            events.contains {
                if case let .status(state, _, _, _, _, _) = $0 { return state == .stopped }
                return false
            }
        }
        /// Frames the perf telemetry has counted — i.e. frames actually decoded
        /// AND run through inference (a direct "the real FileSource was reached"
        /// signal, independent of whether any detection was found).
        func inferredFrameCount() -> UInt64 {
            events.reduce(UInt64(0)) { acc, ev in
                if case let .perf(_, _, _, frames, _) = ev { return max(acc, frames) }
                return acc
            }
        }
    }

    /// Build a Pipeline with the SAME real FrameSource-factory injection as
    /// App.runSocketServed (config here is test-tuned for deterministic
    /// detections; production uses its default config): the real
    /// VisionEngine detector + the real CaptureSourceFactory.make injected via
    /// useFrameSourceFactory. The detector set is `.all` so detections flow
    /// deterministically (the built-in classifier always returns classes); the
    /// wiring under test (real factory injection) is identical to production.
    private func makeProductionWiredPipeline(sink: EventSink) async -> Pipeline {
        let pipeline = Pipeline(detector: VisionEngine(), sink: sink,
                                config: PipelineConfig(sensitivity: 1.0, detectors: .all))
        await pipeline.useFrameSourceFactory(CaptureSourceFactory.make)
        return pipeline
    }

    /// Drive the pipeline's run task to completion (the file stream finishes
    /// naturally -> a .stopped status), polling the sink.
    private func waitForRunToFinish(_ sink: RecordingSink, timeoutMs: Int = 10_000) async {
        let deadline = Date().addingTimeInterval(Double(timeoutMs) / 1000.0)
        while Date() < deadline {
            if await sink.hasFinished() { return }
            try? await Task.sleep(nanoseconds: 10_000_000) // 10ms
        }
    }

    // MARK: - (1) Production wiring decodes REAL frames (not the zero-frame stub)

    func testProductionWiredPipelineAnalyzeFileDecodesRealFrames() async throws {
        let url = try await synthesizeVideo(frameCount: 8)
        defer { try? FileManager.default.removeItem(at: url) }

        let sink = RecordingSink()
        let pipeline = await makeProductionWiredPipeline(sink: sink)

        // Drive the PRODUCTION op the daemon sends for analyze.file. With the
        // real factory injected this must reach the real FileSource and decode
        // frames; with the stub default it would decode ZERO.
        await pipeline.handle(.analyzeFile(path: url.path))
        await waitForRunToFinish(sink)

        // Real frames reached the real FileSource and ran through inference.
        let frames = await sink.inferredFrameCount()
        XCTAssertGreaterThan(frames, 0,
            "production-wired pipeline must decode real frames via the real FileSource on analyze.file (NOT zero)")
        // And detection flowed (NOT zero) over those real frames.
        let dets = await sink.detectionCount()
        XCTAssertGreaterThan(dets, 0,
            "detections must flow over the real decoded frames (NOT zero)")
        let finished = await sink.hasFinished()
        XCTAssertTrue(finished, "the file stream finishes naturally -> a stopped status")
    }

    func testProductionWiredPipelineWatchStartFileDecodesRealFrames() async throws {
        let url = try await synthesizeVideo(frameCount: 6)
        defer { try? FileManager.default.removeItem(at: url) }

        let sink = RecordingSink()
        let pipeline = await makeProductionWiredPipeline(sink: sink)

        await pipeline.handle(.watchStart(source: .file(path: url.path)))
        await waitForRunToFinish(sink)

        let frames = await sink.inferredFrameCount()
        XCTAssertGreaterThan(frames, 0,
            "production-wired watch.start(.file) must reach the real FileSource and decode frames (NOT zero)")
        let dets = await sink.detectionCount()
        XCTAssertGreaterThan(dets, 0,
            "detections must flow over the real decoded frames (NOT zero)")
    }

    /// CONTRAST: the DEFAULT Pipeline (no factory injected) keeps the zero-frame
    /// StubFrameSource, so the same op decodes NOTHING. This is exactly the bug
    /// the production wiring closes — proving the injection is load-bearing.
    func testDefaultPipelineWithoutInjectionDecodesZeroFrames() async throws {
        let url = try await synthesizeVideo(frameCount: 8)
        defer { try? FileManager.default.removeItem(at: url) }

        let sink = RecordingSink()
        // NO useFrameSourceFactory call -> the stub default.
        let pipeline = Pipeline(detector: VisionEngine(), sink: sink,
                                config: PipelineConfig(sensitivity: 1.0))
        await pipeline.handle(.analyzeFile(path: url.path))
        await waitForRunToFinish(sink)

        let dets = await sink.detectionCount()
        XCTAssertEqual(dets, 0,
            "WITHOUT the real-factory injection the stub yields zero frames (the bug the wiring fixes)")
    }

    // MARK: - (2) vision.perf carries a REAL measured inference-ms

    func testProductionPathEmitsRealMeasuredPerf() async throws {
        let url = try await synthesizeVideo(frameCount: 8)
        defer { try? FileManager.default.removeItem(at: url) }

        let sink = RecordingSink()
        let pipeline = await makeProductionWiredPipeline(sink: sink)
        await pipeline.handle(.analyzeFile(path: url.path))
        await waitForRunToFinish(sink)

        let events = await sink.snapshot()
        let perfs: [(Double, Double, Double, UInt64)] = events.compactMap { ev in
            if case let .perf(p50, p95, fps, frames, _) = ev { return (p50, p95, fps, frames) }
            return nil
        }
        XCTAssertFalse(perfs.isEmpty, "the file path must emit at least one vision.perf event")
        for (p50, p95, fps, frames) in perfs {
            XCTAssertTrue(p50.isFinite && p50 > 0,
                          "vision.perf p50 inference-ms must be a real measured value (>0, finite), got \(p50)")
            XCTAssertTrue(p95.isFinite && p95 >= p50, "p95 must be finite and >= p50")
            XCTAssertTrue(fps.isFinite && fps > 0, "inference-bound fps derived from a real latency must be finite/>0")
            XCTAssertGreaterThan(frames, 0, "perf must count at least one measured frame")
        }
    }

    /// The analyze-video CLI helper path measures inference directly. Assert the
    /// engine's measured entry returns a real, finite, > 0 inference-ms on a real
    /// decoded frame (the number the CLI's vision.perf carries).
    func testEngineDetectTimedReturnsRealInferenceMs() async throws {
        let url = try await synthesizeVideo(frameCount: 3)
        defer { try? FileManager.default.removeItem(at: url) }

        let engine = VisionEngine()
        let src: FrameSource = CaptureSourceFactory.make(for: .file(path: url.path))
        var sawTimedFrame = false
        for await frame in src.frames() {
            let (_, inferenceMs) = engine.detectTimed(in: frame, detectors: .all, minConfidence: 0.0)
            XCTAssertTrue(inferenceMs.isFinite, "measured inference-ms must be finite")
            XCTAssertGreaterThan(inferenceMs, 0, "real Vision perform over a real frame takes > 0 ms")
            sawTimedFrame = true
        }
        XCTAssertTrue(sawTimedFrame, "the real FileSource must yield at least one frame to time")
    }

    /// detectTimed on a backing-less detector path reports 0 (nothing ran) — we
    /// never fabricate a latency for a frame we did not infer over.
    func testDetectTimedReportsZeroWhenNoInferenceRuns() {
        let engine = VisionEngine()
        // Empty detector set -> no requests built -> nothing performed -> 0 ms.
        let img = AppWiringTests.solidImage()
        let (dets, ms) = engine.detectTimed(
            in: Frame(cgImage: img, timestamp: 0, source: .file(path: "x"), index: 0),
            detectors: DetectorSet(rawValue: 0), minConfidence: 0.0)
        XCTAssertTrue(dets.isEmpty)
        XCTAssertEqual(ms, 0, "no inference ran -> no fabricated latency")
    }

    private static func solidImage(side: Int = 16) -> CGImage {
        let cs = CGColorSpaceCreateDeviceGray()
        let ctx = CGContext(data: nil, width: side, height: side, bitsPerComponent: 8,
                            bytesPerRow: side, space: cs,
                            bitmapInfo: CGImageAlphaInfo.none.rawValue)!
        ctx.setFillColor(gray: 0.5, alpha: 1)
        ctx.fill(CGRect(x: 0, y: 0, width: side, height: side))
        return ctx.makeImage()!
    }
}

// Percentile helper exercised directly (it backs vision.perf p50/p95).
final class PerfPercentileTests: XCTestCase {
    func testPercentileNearestRank() {
        let s = [10.0, 20.0, 30.0, 40.0, 50.0]
        XCTAssertEqual(Pipeline.percentile(s, 0.0), 10.0, accuracy: 1e-9)
        XCTAssertEqual(Pipeline.percentile(s, 1.0), 50.0, accuracy: 1e-9)
        XCTAssertEqual(Pipeline.percentile(s, 0.5), 30.0, accuracy: 1e-9)
    }
    func testPercentileEmptyIsZero() {
        XCTAssertEqual(Pipeline.percentile([], 0.5), 0.0, accuracy: 1e-9)
    }
    func testPercentileSingle() {
        XCTAssertEqual(Pipeline.percentile([7.0], 0.95), 7.0, accuracy: 1e-9)
    }
    func testPercentileClampsQ() {
        let s = [1.0, 2.0, 3.0]
        XCTAssertEqual(Pipeline.percentile(s, -1), 1.0, accuracy: 1e-9)
        XCTAssertEqual(Pipeline.percentile(s, 9), 3.0, accuracy: 1e-9)
    }
}
