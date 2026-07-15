// Capture.swift — CAPTURE module (filled by the capture agent).
//
// Responsibility: produce a stream of Frames from a CaptureSource:
//   - .camera : AVFoundation AVCaptureSession from the user's OWN camera.
//   - .screen : ScreenCaptureKit SCStream of the user's OWN screen.
//   - .file   : AVAssetReader over a user-provided video under videos/input
//               (this is the ONLY path verifiable headlessly — no TCC).
//
// macOS TCC is the REAL gate for .camera (Camera) and .screen (Screen
// Recording): it needs runtime USER CONSENT and is NOT grantable by SBPL. The
// capture agent must check authorization (AVCaptureDevice.authorizationStatus /
// SCShareableContent) and emit a vision.error + vision.status when denied —
// NEVER attempt covert capture. HARD RULE for this build: do not actually open
// the camera or start a real screen capture; verify .file + the auth-gating
// logic only, with synthesized/provided test video + unit tests.
//
// This file provides the PUBLIC SEAM (FrameSource protocol + an AsyncStream of
// Frames + an authorization probe) and the three concrete implementations.
//
// DEFENSIVE-ONLY invariants enforced here:
//   - Authorized-only + the user's OWN devices. Camera/screen capture is NEVER
//     started without a confirmed `.authorized` status; a `.denied`/`.restricted`/
//     `.notDetermined` probe yields an empty stream (the pipeline emits the
//     error/status) — NEVER covert capture.
//   - Pixels stay in-process. Frames carry a CVPixelBuffer/CGImage that never
//     leave the device; nothing here writes pixels to disk or the network.
//   - No identity. Capture only produces raw frames; WHO is never computed —
//     not here, not anywhere (there is no API for it).

import Foundation
import AVFoundation
import ScreenCaptureKit
import CoreVideo
import CoreMedia
import CoreGraphics

// ===========================================================================
// Seam: CaptureAuthorization + FrameSource
// ===========================================================================

/// Authorization state for a TCC-gated capture source.
public enum CaptureAuthorization: String, Sendable, Equatable {
    case authorized
    case denied
    case notDetermined
    case restricted
    case notApplicable   // .file needs no TCC
}

/// A source of Frames. Implementations: CameraSource, ScreenSource, FileSource.
public protocol FrameSource: Sendable {
    /// The source this produces frames from.
    var source: CaptureSource { get }

    /// Probe TCC authorization WITHOUT starting capture. For .file this is
    /// `.notApplicable`. The app must honor `.denied` (emit error, do not capture).
    func authorization() async -> CaptureAuthorization

    /// An async stream of Frames. Starting the stream is what actually opens the
    /// device (only after authorization is confirmed). Finishing the stream
    /// (or cancelling the consuming task) stops capture.
    func frames() -> AsyncStream<Frame>
}

/// Stub frame source — never authorized, emits no frames. Kept for the seam /
/// for wiring tests; the real sources below replace it in App.swift. Present so
/// the package links and pipeline/main compile.
public struct StubFrameSource: FrameSource {
    public let source: CaptureSource
    public init(source: CaptureSource) { self.source = source }

    public func authorization() async -> CaptureAuthorization {
        switch source {
        case .file: return .notApplicable
        case .camera, .screen: return .notDetermined
        }
    }

    public func frames() -> AsyncStream<Frame> {
        AsyncStream { continuation in
            // Stub: emit nothing, finish immediately.
            continuation.finish()
        }
    }
}

// ===========================================================================
// Shared helper: map an AVFoundation video-auth status -> CaptureAuthorization
// ===========================================================================

@inline(__always)
func captureAuthorization(from status: AVAuthorizationStatus) -> CaptureAuthorization {
    switch status {
    case .authorized:    return .authorized
    case .denied:        return .denied
    case .restricted:    return .restricted
    case .notDetermined: return .notDetermined
    @unknown default:    return .notDetermined
    }
}

// ===========================================================================
// FileSource — AVAssetReader over a user-provided video file (HEADLESS-OK)
// ===========================================================================
//
// The ONLY headlessly-verifiable source: no camera, no screen, no TCC. Decodes
// frames from a video file the user placed under apps/vision/videos/input (the
// manifest's fs_read scope) and hands each decoded frame to the pipeline as a
// CVPixelBuffer-backed Frame. Used by both `analyze.file` (one pass over a file)
// and `watch.start(source: .file(path:))`.

/// Decodes frames from a video file via AVAssetReader. Authorized-implicitly
/// (`.notApplicable`): a file the user provided needs no device consent.
public struct FileSource: FrameSource {
    public let source: CaptureSource

    /// The file path being decoded (mirror of `source`'s associated value).
    public let path: String

    /// Decode at most this many frames (0 = unlimited). A safety cap so a huge
    /// file can't pin the pipeline forever; the pipeline also throttles.
    public let maxFrames: UInt64

    /// Output pixel format. 32BGRA is what Vision's request handler ingests
    /// most cheaply and what CVPixelBuffer-backed Frames carry downstream.
    private let pixelFormat: OSType = kCVPixelFormatType_32BGRA

    public init(path: String, maxFrames: UInt64 = 0) {
        self.path = path
        self.source = .file(path: path)
        self.maxFrames = maxFrames
    }

    /// Convenience init when the caller already has a `.file` CaptureSource.
    /// Non-file sources are a programmer error here; we still degrade safely to
    /// an empty path so `frames()` finishes immediately rather than crashing.
    public init(source: CaptureSource, maxFrames: UInt64 = 0) {
        if case let .file(p) = source {
            self.init(path: p, maxFrames: maxFrames)
        } else {
            self.init(path: "", maxFrames: maxFrames)
        }
    }

    /// A user-provided file needs no TCC.
    public func authorization() async -> CaptureAuthorization { .notApplicable }

    public func frames() -> AsyncStream<Frame> {
        let path = self.path
        let source = self.source
        let pixelFormat = self.pixelFormat
        let maxFrames = self.maxFrames

        return AsyncStream { continuation in
            // Decode off the calling task; cancellation of the consuming task
            // tears the stream down (we check Task.isCancelled each frame).
            let task = Task.detached(priority: .userInitiated) {
                await FileSource.decode(
                    path: path,
                    source: source,
                    pixelFormat: pixelFormat,
                    maxFrames: maxFrames,
                    continuation: continuation
                )
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    /// The actual decode loop. Reads sample buffers from the file's first video
    /// track, converts each to a CVPixelBuffer-backed Frame, and yields it.
    /// Totally non-throwing to the caller: any error simply finishes the stream
    /// (the pipeline surfaces a vision.error from the empty/short result).
    private static func decode(
        path: String,
        source: CaptureSource,
        pixelFormat: OSType,
        maxFrames: UInt64,
        continuation: AsyncStream<Frame>.Continuation
    ) async {
        guard !path.isEmpty else { continuation.finish(); return }

        let url = URL(fileURLWithPath: path)
        let asset = AVURLAsset(url: url)

        let reader: AVAssetReader
        let output: AVAssetReaderTrackOutput
        do {
            let videoTracks = try await asset.loadTracks(withMediaType: .video)
            guard let track = videoTracks.first else {
                // No video track -> nothing to decode.
                continuation.finish(); return
            }
            reader = try AVAssetReader(asset: asset)
            let outputSettings: [String: Any] = [
                kCVPixelBufferPixelFormatTypeKey as String: Int(pixelFormat)
            ]
            output = AVAssetReaderTrackOutput(track: track, outputSettings: outputSettings)
            output.alwaysCopiesSampleData = false
            guard reader.canAdd(output) else { continuation.finish(); return }
            reader.add(output)
            guard reader.startReading() else { continuation.finish(); return }
        } catch {
            // Unreadable/corrupt file, missing path, unsupported codec, etc.
            continuation.finish(); return
        }

        var index: UInt64 = 0
        // Decode time of the first frame, so timestamps are run-relative.
        var firstPTS: TimeInterval? = nil

        while !Task.isCancelled {
            guard let sample = output.copyNextSampleBuffer() else { break }
            defer { /* sample is released at scope end */ }

            guard let imageBuffer = CMSampleBufferGetImageBuffer(sample) else {
                continue   // skip non-image samples
            }
            // CMSampleBufferGetImageBuffer returns a CVImageBuffer; for video
            // tracks it is a CVPixelBuffer.
            let pixelBuffer = imageBuffer as CVPixelBuffer

            let ptsTime = CMSampleBufferGetPresentationTimeStamp(sample)
            let pts = CMTIME_IS_NUMERIC(ptsTime) ? ptsTime.seconds : Double(index)
            if firstPTS == nil { firstPTS = pts }
            let ts = pts - (firstPTS ?? 0)

            let frame = Frame(pixelBuffer: pixelBuffer, timestamp: ts,
                              source: source, index: index)
            continuation.yield(frame)
            index &+= 1

            if maxFrames != 0 && index >= maxFrames { break }
        }

        if reader.status == .reading { reader.cancelReading() }
        continuation.finish()
    }
}

// ===========================================================================
// CameraSource — AVFoundation AVCaptureSession (DEVICE-GATED, TCC-consented)
// ===========================================================================
//
// DEVICE-GATED: opening the camera requires macOS TCC "Camera" consent at
// RUNTIME and real hardware. It is NOT exercised in tests and is NOT started
// here without a confirmed `.authorized` status. Authorized-only + the user's
// OWN camera; NEVER covert capture. The session delegate converts each captured
// CMSampleBuffer (CVPixelBuffer) into a Frame.

/// Live capture from the user's OWN camera. DEVICE-GATED (TCC: Camera).
public final class CameraSource: NSObject, FrameSource, @unchecked Sendable {
    public let source: CaptureSource = .camera

    /// Capture preset; medium keeps inference cheap on the ANE/GPU.
    private let sessionPreset: AVCaptureSession.Preset
    private let session = AVCaptureSession()
    private let sampleQueue = DispatchQueue(label: "darwin.vision.camera.samples")

    /// Continuation for the live frame stream; set when `frames()` starts.
    private let stateLock = NSLock()
    private var continuation: AsyncStream<Frame>.Continuation?
    private var index: UInt64 = 0
    private var firstTS: TimeInterval? = nil

    public init(sessionPreset: AVCaptureSession.Preset = .medium) {
        self.sessionPreset = sessionPreset
        super.init()
    }

    /// Probe TCC WITHOUT opening the device. The pipeline decides whether to
    /// prompt (requestAccess) or honor a denial.
    public func authorization() async -> CaptureAuthorization {
        captureAuthorization(from: AVCaptureDevice.authorizationStatus(for: .video))
    }

    /// Request TCC consent (prompts on first use). DEVICE-GATED — calling this
    /// surfaces the system permission dialog. Returns the resulting state.
    /// Not used in tests (no TCC in CI).
    public func requestAuthorization() async -> CaptureAuthorization {
        let current = AVCaptureDevice.authorizationStatus(for: .video)
        if current == .notDetermined {
            let granted = await AVCaptureDevice.requestAccess(for: .video)
            return granted ? .authorized : .denied
        }
        return captureAuthorization(from: current)
    }

    public func frames() -> AsyncStream<Frame> {
        AsyncStream { continuation in
            // DEFENSIVE GATE: never open the camera without confirmed consent.
            guard AVCaptureDevice.authorizationStatus(for: .video) == .authorized else {
                continuation.finish()
                return
            }
            self.stateLock.lock()
            self.continuation = continuation
            self.index = 0
            self.firstTS = nil
            self.stateLock.unlock()

            guard self.configureSession() else {
                continuation.finish()
                return
            }

            continuation.onTermination = { [weak self] _ in
                self?.stop()
            }
            // session.startRunning blocks briefly; do it off the stream-start.
            self.sampleQueue.async { [weak self] in
                self?.session.startRunning()
            }
        }
    }

    /// Build the capture graph: default video device -> session -> sample-buffer
    /// output on `sampleQueue`. Returns false if the device is unavailable.
    private func configureSession() -> Bool {
        session.beginConfiguration()
        defer { session.commitConfiguration() }
        if session.canSetSessionPreset(sessionPreset) {
            session.sessionPreset = sessionPreset
        }

        guard let device = AVCaptureDevice.default(for: .video),
              let input = try? AVCaptureDeviceInput(device: device),
              session.canAddInput(input) else {
            return false
        }
        session.addInput(input)

        let output = AVCaptureVideoDataOutput()
        output.videoSettings = [
            kCVPixelBufferPixelFormatTypeKey as String: Int(kCVPixelFormatType_32BGRA)
        ]
        output.alwaysDiscardsLateVideoFrames = true
        output.setSampleBufferDelegate(self, queue: sampleQueue)
        guard session.canAddOutput(output) else { return false }
        session.addOutput(output)
        return true
    }

    /// Stop the session and finish the stream. Idempotent.
    private func stop() {
        if session.isRunning { session.stopRunning() }
        stateLock.lock()
        let cont = continuation
        continuation = nil
        stateLock.unlock()
        cont?.finish()
    }
}

extension CameraSource: AVCaptureVideoDataOutputSampleBufferDelegate {
    public func captureOutput(_ output: AVCaptureOutput,
                              didOutput sampleBuffer: CMSampleBuffer,
                              from connection: AVCaptureConnection) {
        guard let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        let ptsTime = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        let pts = CMTIME_IS_NUMERIC(ptsTime) ? ptsTime.seconds : 0

        stateLock.lock()
        let cont = continuation
        if firstTS == nil { firstTS = pts }
        let ts = pts - (firstTS ?? 0)
        let idx = index
        index &+= 1
        stateLock.unlock()

        cont?.yield(Frame(pixelBuffer: pixelBuffer, timestamp: ts,
                          source: .camera, index: idx))
    }
}

// ===========================================================================
// ScreenSource — ScreenCaptureKit SCStream (DEVICE-GATED, TCC-consented)
// ===========================================================================
//
// DEVICE-GATED: capturing the screen requires macOS TCC "Screen Recording"
// consent at RUNTIME. It is NOT exercised in tests and is NOT started without a
// confirmed authorization. Authorized-only + the user's OWN screen (the main
// display); NEVER covert capture. The SCStreamOutput callback converts each
// CMSampleBuffer (CVPixelBuffer) into a Frame.

/// Live capture of the user's OWN screen. DEVICE-GATED (TCC: Screen Recording).
public final class ScreenSource: NSObject, FrameSource, @unchecked Sendable {
    public let source: CaptureSource = .screen

    /// Target capture FPS (kept modest; the pipeline throttles events anyway).
    private let fps: Int

    private let sampleQueue = DispatchQueue(label: "darwin.vision.screen.samples")
    private let stateLock = NSLock()
    private var stream: SCStream?
    private var continuation: AsyncStream<Frame>.Continuation?
    private var index: UInt64 = 0
    private var firstTS: TimeInterval? = nil

    public init(fps: Int = 10) {
        self.fps = max(1, fps)
        super.init()
    }

    /// Probe Screen Recording authorization WITHOUT capturing. SCShareableContent
    /// succeeds only when authorized (or prompts on first use), so we treat a
    /// successful enumeration as `.authorized` and a failure as `.denied`. There
    /// is no synchronous status API equivalent to AVCaptureDevice's.
    public func authorization() async -> CaptureAuthorization {
        do {
            // excludingDesktopWindows:false is the cheapest enumeration; a
            // throw here means the user has not granted Screen Recording.
            _ = try await SCShareableContent.excludingDesktopWindows(false,
                                                                     onScreenWindowsOnly: true)
            return .authorized
        } catch {
            return .denied
        }
    }

    public func frames() -> AsyncStream<Frame> {
        AsyncStream { continuation in
            self.stateLock.lock()
            self.continuation = continuation
            self.index = 0
            self.firstTS = nil
            self.stateLock.unlock()

            continuation.onTermination = { [weak self] _ in
                self?.stop()
            }

            // DEVICE-GATED start: enumerate shareable content (requires consent),
            // build a single-display filter, and start the stream. If consent is
            // absent the enumeration throws and we finish the stream cleanly.
            Task { [weak self] in
                await self?.start(continuation: continuation)
            }
        }
    }

    private func start(continuation: AsyncStream<Frame>.Continuation) async {
        do {
            let content = try await SCShareableContent.excludingDesktopWindows(
                false, onScreenWindowsOnly: true)
            guard let display = content.displays.first else {
                continuation.finish(); return
            }
            // The user's OWN main display, with all windows of that display.
            let filter = SCContentFilter(display: display, excludingWindows: [])

            let config = SCStreamConfiguration()
            config.width = display.width
            config.height = display.height
            config.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fps))
            config.pixelFormat = kCVPixelFormatType_32BGRA
            config.queueDepth = 5

            let stream = SCStream(filter: filter, configuration: config, delegate: nil)
            try stream.addStreamOutput(self, type: .screen,
                                       sampleHandlerQueue: sampleQueue)
            // Store the stream via a SYNC helper: NSLock.lock()/unlock() are not
            // callable from an async context under the Swift 6 language mode.
            setStream(stream)
            try await stream.startCapture()
        } catch {
            // Consent denied / no display / SCStream failure -> finish cleanly.
            continuation.finish()
        }
    }

    /// SYNC helper to publish the started stream under the lock (the lock cannot
    /// be taken directly from `start`'s async context under Swift 6).
    private func setStream(_ s: SCStream) {
        stateLock.lock()
        self.stream = s
        stateLock.unlock()
    }

    private func stop() {
        stateLock.lock()
        let s = stream
        stream = nil
        let cont = continuation
        continuation = nil
        stateLock.unlock()
        if let s = s {
            Task { try? await s.stopCapture() }
        }
        cont?.finish()
    }
}

extension ScreenSource: SCStreamOutput {
    /// True only for `.complete` SCStream frames (ones carrying fresh pixels).
    static func isCompleteFrame(_ sb: CMSampleBuffer) -> Bool {
        guard let arr = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false)
                as? [[SCStreamFrameInfo: Any]],
              let attachments = arr.first,
              let rawStatus = attachments[.status] as? Int,
              let status = SCFrameStatus(rawValue: rawStatus) else {
            return false
        }
        return status == .complete
    }

    public func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
                       of type: SCStreamOutputType) {
        guard type == .screen else { return }
        // SCStream delivers .idle/.blank/.suspended frames when the screen is
        // not changing; only forward .complete frames with real pixels.
        guard sampleBuffer.isValid,
              ScreenSource.isCompleteFrame(sampleBuffer),
              let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }

        let ptsTime = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        let pts = CMTIME_IS_NUMERIC(ptsTime) ? ptsTime.seconds : 0

        stateLock.lock()
        let cont = continuation
        if firstTS == nil { firstTS = pts }
        let ts = pts - (firstTS ?? 0)
        let idx = index
        index &+= 1
        stateLock.unlock()

        cont?.yield(Frame(pixelBuffer: pixelBuffer, timestamp: ts,
                          source: .screen, index: idx))
    }
}

// ===========================================================================
// Factory — build the right FrameSource for a CaptureSource
// ===========================================================================

public enum CaptureSourceFactory {
    /// Make the concrete FrameSource for a CaptureSource. The pipeline/main use
    /// this so they don't hardcode which implementation backs which source.
    public static func make(for source: CaptureSource) -> FrameSource {
        switch source {
        case .camera:            return CameraSource()
        case .screen:            return ScreenSource()
        case let .file(path):    return FileSource(path: path)
        }
    }
}
