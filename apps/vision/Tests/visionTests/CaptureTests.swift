// CaptureTests.swift — CAPTURE module tests.
//
// Headlessly verifiable scope ONLY: FileSource frame extraction, the
// authorization mapping, the factory, and the safe-degrade paths. We synthesize
// a tiny video file with AVAssetWriter at runtime (no checked-in binary asset)
// and assert FileSource decodes Frames from it.
//
// DEVICE-GATED, NOT EXERCISED HERE: CameraSource (AVFoundation, TCC: Camera) and
// ScreenSource (ScreenCaptureKit, TCC: Screen Recording) need runtime user
// consent + real hardware. We assert ONLY their non-capturing behavior — that a
// probe never opens a device and that frames() finishes cleanly without consent
// — and NEVER call startRunning()/startCapture(). No camera, no screen, no TCC.

import XCTest
import Foundation
import AVFoundation
import CoreVideo
import CoreMedia
@testable import vision

final class FileSourceTests: XCTestCase {

    // MARK: - Synthesized test video helper

    /// Synthesize a tiny H.264 .mov with `frameCount` distinct gray frames.
    /// Returns the file URL; the caller removes it. Throws (skipping the test)
    /// if the platform can't encode video in this environment.
    private func synthesizeVideo(frameCount: Int, width: Int = 64, height: Int = 48,
                                 fps: Int32 = 30) async throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("darwin_vision_test_\(UUID().uuidString).mov")
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
            guard let pb = Self.makePixelBuffer(width: width, height: height,
                                                gray: UInt8(40 + (i * 20) % 200)) else {
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

    /// Allocate a solid-gray 32ARGB CVPixelBuffer.
    private static func makePixelBuffer(width: Int, height: Int, gray: UInt8) -> CVPixelBuffer? {
        var pb: CVPixelBuffer?
        let attrs: [String: Any] = [
            kCVPixelBufferCGImageCompatibilityKey as String: true,
            kCVPixelBufferCGBitmapContextCompatibilityKey as String: true,
        ]
        let rc = CVPixelBufferCreate(kCFAllocatorDefault, width, height,
                                     kCVPixelFormatType_32ARGB, attrs as CFDictionary, &pb)
        guard rc == kCVReturnSuccess, let pb = pb else { return nil }
        CVPixelBufferLockBaseAddress(pb, [])
        if let base = CVPixelBufferGetBaseAddress(pb) {
            let bpr = CVPixelBufferGetBytesPerRow(pb)
            memset(base, Int32(gray), bpr * height)
        }
        CVPixelBufferUnlockBaseAddress(pb, [])
        return pb
    }

    /// Drain a FrameSource's stream into an array (with a hard cap so a
    /// misbehaving source can't hang the test).
    private func collectFrames(_ src: FrameSource, cap: Int = 1000) async -> [Frame] {
        var out: [Frame] = []
        for await frame in src.frames() {
            out.append(frame)
            if out.count >= cap { break }
        }
        return out
    }

    // MARK: - The headless core: FileSource decodes frames

    func testFileSourceDecodesAllFrames() async throws {
        let expected = 8
        let url = try await synthesizeVideo(frameCount: expected)
        defer { try? FileManager.default.removeItem(at: url) }

        let src = FileSource(path: url.path)
        let frames = await collectFrames(src)

        XCTAssertEqual(frames.count, expected,
                       "FileSource should decode exactly the frames in the file")
        // Every frame is file-sourced, pixel-backed, sized, and monotonically indexed.
        for (i, f) in frames.enumerated() {
            XCTAssertEqual(f.source, .file(path: url.path))
            XCTAssertEqual(f.source.tag, "file")
            XCTAssertNotNil(f.pixelBuffer, "file frames are CVPixelBuffer-backed")
            XCTAssertNil(f.cgImage)
            XCTAssertEqual(f.index, UInt64(i), "frame indices are monotonic from 0")
            XCTAssertEqual(f.pixelSize, CGSize(width: 64, height: 48))
        }
        // First frame timestamp is run-relative (0); timestamps are non-decreasing.
        XCTAssertEqual(frames.first?.timestamp ?? -1, 0, accuracy: 1e-6)
        for i in 1..<frames.count {
            XCTAssertGreaterThanOrEqual(frames[i].timestamp, frames[i - 1].timestamp)
        }
    }

    func testFileSourceRespectsMaxFrames() async throws {
        let url = try await synthesizeVideo(frameCount: 10)
        defer { try? FileManager.default.removeItem(at: url) }

        let src = FileSource(path: url.path, maxFrames: 3)
        let frames = await collectFrames(src)
        XCTAssertEqual(frames.count, 3, "maxFrames caps the decode")
        XCTAssertEqual(frames.map(\.index), [0, 1, 2])
    }

    func testFileSourceConvenienceInitFromSource() async throws {
        let url = try await synthesizeVideo(frameCount: 4)
        defer { try? FileManager.default.removeItem(at: url) }

        let src = FileSource(source: .file(path: url.path))
        XCTAssertEqual(src.path, url.path)
        let frames = await collectFrames(src)
        XCTAssertEqual(frames.count, 4)
    }

    // MARK: - Safe-degrade paths (no crash, empty stream)

    func testFileSourceMissingFileFinishesEmpty() async {
        let src = FileSource(path: "/nonexistent/does-not-exist.mov")
        let frames = await collectFrames(src)
        XCTAssertTrue(frames.isEmpty, "an unreadable path yields no frames, no crash")
    }

    func testFileSourceEmptyPathFinishesEmpty() async {
        let src = FileSource(path: "")
        let frames = await collectFrames(src)
        XCTAssertTrue(frames.isEmpty)
    }

    func testFileSourceFromNonFileSourceDegradesToEmptyPath() async {
        // Programmer-error guard: a non-file CaptureSource yields an empty path
        // (so frames() finishes immediately) instead of crashing.
        let src = FileSource(source: .camera)
        XCTAssertEqual(src.path, "")
        let frames = await collectFrames(src)
        XCTAssertTrue(frames.isEmpty)
    }

    func testFileSourceAuthorizationIsNotApplicable() async {
        let src = FileSource(path: "x.mov")
        let auth = await src.authorization()
        XCTAssertEqual(auth, .notApplicable, "a user-provided file needs no TCC")
        XCTAssertEqual(src.source, .file(path: "x.mov"))
    }
}

/// Authorization-mapping + factory + device-gated NON-capture behavior.
/// No device is ever opened here.
final class CaptureAuthAndFactoryTests: XCTestCase {

    func testAuthStatusMapping() {
        XCTAssertEqual(captureAuthorization(from: .authorized), .authorized)
        XCTAssertEqual(captureAuthorization(from: .denied), .denied)
        XCTAssertEqual(captureAuthorization(from: .restricted), .restricted)
        XCTAssertEqual(captureAuthorization(from: .notDetermined), .notDetermined)
    }

    func testFactoryBuildsMatchingSource() {
        XCTAssertEqual(CaptureSourceFactory.make(for: .camera).source, .camera)
        XCTAssertEqual(CaptureSourceFactory.make(for: .screen).source, .screen)
        let fileSrc = CaptureSourceFactory.make(for: .file(path: "videos/input/a.mov"))
        XCTAssertEqual(fileSrc.source, .file(path: "videos/input/a.mov"))
        XCTAssertTrue(fileSrc is FileSource)
    }

    func testCameraSourceProbeDoesNotOpenDevice() async {
        // authorization() must only READ the TCC status, never start a session.
        // On CI (no consent) this is .notDetermined/.denied/.restricted; we only
        // assert it returns a value WITHOUT capturing. We never call frames().
        let cam = CameraSource()
        XCTAssertEqual(cam.source, .camera)
        let auth = await cam.authorization()
        XCTAssertNotEqual(auth, .notApplicable, "camera is always a TCC-gated source")
    }

    func testCameraSourceWithoutAuthEmitsNoFrames() async throws {
        // DEFENSIVE GATE: frames() must finish immediately (no capture) when the
        // camera is not authorized. In an unattended test there is no consent,
        // so this exercises the gate without ever opening hardware.
        let cam = CameraSource()
        let camAuth = await cam.authorization()
        guard camAuth != .authorized else {
            throw XCTSkip("camera is authorized in this environment; skip the no-consent gate test")
        }
        var produced = 0
        for await _ in cam.frames() { produced += 1; if produced > 0 { break } }
        XCTAssertEqual(produced, 0, "no consent -> no frames, no covert capture")
    }

    func testScreenSourceSourceTag() {
        let screen = ScreenSource()
        XCTAssertEqual(screen.source, .screen)
        XCTAssertEqual(screen.source.tag, "screen")
    }

    func testStubFrameSourceStillSatisfiesSeam() async {
        // The stub remains a valid FrameSource for wiring tests.
        let stub = StubFrameSource(source: .file(path: "x"))
        let stubAuth = await stub.authorization()
        XCTAssertEqual(stubAuth, .notApplicable)
        var n = 0
        for await _ in stub.frames() { n += 1 }
        XCTAssertEqual(n, 0)
    }
}
