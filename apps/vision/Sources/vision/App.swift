// App.swift — entry point for the Vision micro-app binary (manifest entry =
// "vision"). MAIN/wiring module (ipc_main agent), built on the INFERENCE agent's
// VisionEngine.
//
// NOTE: this file is App.swift, NOT main.swift — SwiftPM treats a file literally
// named main.swift as top-level code, which collides with @main. The @main
// async entry point lives here instead.
//
// THREE run modes (the binary chooses by argv):
//   1. socket-served runtime (default, no args): the daemon launches us under
//      sandbox-exec with JARVIS_APP_TOKEN / _SOCKET / _NAME in the env. We
//      connect to the per-app Unix socket, pump host Ops into the Pipeline, and
//      emit token-stamped vision.* telemetry until the host stops us / the
//      socket closes. This is the path daemon/src/apps.rs drives.
//   2. `vision analyze <imagepath>`  — headless CLI: load ONE image, run the
//      built-in Vision requests via VisionEngine once, print the detection
//      summary as JSON. NO camera/screen/TCC/socket — the proof the pipeline is
//      real (mirrors the de-risk probe).
//   3. `vision analyze-video <file>` — headless CLI: decode a CONFINED video
//      file (under apps/vision/videos/input) through the REAL FileSource
//      (CaptureSourceFactory.make), run detection per frame, print a summary
//      with REAL measured per-frame inference latency. .file is the ONLY capture
//      path verifiable headlessly (no camera/screen/TCC); camera/screen capture,
//      ANE residency, and real-time capture fps are DEVICE-GATED and NOT claimed
//      here.
//
// Boot sequence (socket mode, all on-device, offline):
//   1. Read the launch env (JARVIS_APP_TOKEN / _SOCKET / _NAME) into AppEnv. On
//      a missing var, log to stderr (relayed as app.log) and exit non-zero —
//      never run without a token (every app->host line must be stamped).
//   2. Build the wiring: VisionEngine (built-in Vision, ANE/GPU, offline) ->
//      OutboundSink over the real socket LineWriter (token-stamped) -> Pipeline.
//   3. Open the AppConnection and pump host Ops into the Pipeline until the
//      connection closes / the host stops us.
//
// DEFENSIVE: we NEVER auto-start the camera/screen. Capture begins only when the
// host sends an explicit watch.start op (dispatched into the Pipeline). The
// socket connection here CONNECTS (binds nothing), opens no window, touches no
// GPU directly, and plays no audio. File-bearing ops are path-confined to the
// granted videos/input dir BEFORE the Pipeline opens anything.

import Foundation

@main
struct VisionApp {
    static func main() async {
        let args = CommandLine.arguments

        // 0. Headless CLI modes — NO camera/screen/TCC/socket/token.
        if args.count >= 2, args[1] == "analyze" {
            exit(runAnalyzeImageCLI(args: Array(args.dropFirst(2))))
        }
        if args.count >= 2, args[1] == "analyze-video" {
            exit(await runAnalyzeVideoCLI(args: Array(args.dropFirst(2))))
        }
        if args.count >= 2, args[1] == "ocr" {
            exit(runOCRImageCLI(args: Array(args.dropFirst(2))))
        }
        if args.count >= 2, args[1] == "handwriting" {
            exit(runHandwritingImageCLI(args: Array(args.dropFirst(2))))
        }
        if args.count >= 2, args[1] == "scan" {
            exit(runScanDocumentCLI(args: Array(args.dropFirst(2))))
        }
        if args.count >= 2, args[1] == "classify-sound" {
            exit(runClassifySoundCLI(args: Array(args.dropFirst(2))))
        }
        if args.count >= 2, ["help", "--help", "-h"].contains(args[1]) {
            printUsage()
            exit(0)
        }

        // 1..3 — socket-served runtime (the daemon-launched path).
        await runSocketServed()
    }

    // -----------------------------------------------------------------------
    // Mode 1 — socket-served runtime (the daemon-launched path).
    // -----------------------------------------------------------------------

    static func runSocketServed() async {
        // 1. Launch env (token + socket). Missing env -> clean non-zero exit.
        //    The app only runs under jarvisd; an accidental standalone launch
        //    (no socket/token) exits 2, binding nothing (mirrors silicon-canvas).
        let env: AppEnv
        do {
            env = try AppEnv.loadFromProcess()
        } catch {
            FileHandle.standardError.write(Data(
                "vision: \(error) (this app runs under jarvisd, not standalone; or use `vision analyze <imagepath>`)\n".utf8))
            exit(2)
        }

        // 2. Open the REAL socket connection. A connect failure means we are not
        //    actually under the daemon — exit 2 rather than spin. The connection
        //    is SPLIT into an independent reader + writer over the same socket (a
        //    dup of the fd), so a blocking read (waiting for the next host op)
        //    never serializes behind app->host telemetry writes.
        let connection: AppConnection
        let writer: LineWriter
        #if canImport(Darwin)
        do {
            let conn = try SocketConnection(env: env)
            connection = SocketAppConnection(reader: conn.reader)
            writer = SocketLineWriter(writer: conn.writer)
        } catch {
            FileHandle.standardError.write(Data("vision: cannot connect to app socket: \(error)\n".utf8))
            exit(2)
        }
        #else
        connection = StubAppConnection(env: env)
        writer = StubLineWriter()
        #endif

        // 3. Wiring: built-in Vision engine -> token-stamping sink -> Pipeline.
        //    The capture seam (file/camera/screen) is owned by the Pipeline; we
        //    only confine file paths before handing the op down.
        let detector: Detector = VisionEngine()
        let sink: EventSink = OutboundSink(token: env.token, writer: writer)
        let pipeline = Pipeline(detector: detector, sink: sink)

        // Inject the REAL FrameSource factory (camera/screen/FileSource). Without
        // this the Pipeline keeps its zero-frame StubFrameSource default, so
        // watch.start / analyze.file would decode NOTHING in the live daemon-
        // launched runtime. This mirrors the analyze-video CLI sibling, which
        // builds its FileSource via the same CaptureSourceFactory.make. The seam
        // is still honored: the Pipeline opens nothing until a watch/analyze op
        // arrives, and camera/screen remain TCC-gated inside their sources.
        await pipeline.useFrameSourceFactory(CaptureSourceFactory.make)

        // Announce we're up (idle) on vision.status.
        await sink.emit(.status(state: .idle, source: nil, sensitivity: 0.5,
                                cameraAuthorized: nil, screenAuthorized: nil,
                                message: "vision app started"))

        // Register the dyld watcher FIRST, then snapshot — so a dlopen in the gap
        // between the snapshot and registration isn't missed (Vision / ScreenCapture
        // Kit / CoreML load lazily when capture STARTS, after the report below).
        DyldReport.watch()
        // Attest our own loaded dyld modules once at startup (READ-ONLY, best-effort).
        // The daemon seeds a trust-on-first-use baseline from this and flags any
        // module a later report adds (injection / unexpected dlopen) — introspect.rs.
        // The op loop below re-attests when the watch flag fires (after each host op).
        await sink.emit(.modules(DyldReport.collectLoadedModules()))

        // The path resolver for analyze.file / watch.start(file:). The daemon
        // runs us with cwd = project root.
        let resolver = VideoPathResolver(projectRoot: FileManager.default.currentDirectoryPath)

        // 4. Pump host ops into the pipeline. File-bearing ops are confined HERE
        //    (before the Pipeline opens anything) so a traversal/escape is a
        //    clean vision.error rather than an EPERM surprise inside capture.
        do {
            try await connection.run { op in
                guard let safeOp = await Self.confineFileOp(op, resolver: resolver, sink: sink) else {
                    return // rejected -> vision.error already emitted
                }
                await pipeline.handle(safeOp)
                // If handling this op lazy-loaded new frameworks (capture start
                // pulls in Vision/SCK/CoreML), re-attest the module set. READ-ONLY.
                if DyldReport.consumeChanged() {
                    await sink.emit(.modules(DyldReport.collectLoadedModules()))
                }
            }
        } catch {
            FileHandle.standardError.write(Data("vision: connection error: \(error)\n".utf8))
            exit(1)
        }
    }

    /// Confine the path on any file-bearing op (analyze.file / watch.start with a
    /// file source) to the granted videos/input dir BEFORE the Pipeline opens it.
    /// Returns the op with its path rewritten to the canonical confined path, or
    /// nil after emitting a vision.error when the path is refused. Non-file ops
    /// pass through unchanged.
    static func confineFileOp(_ op: Op, resolver: VideoPathResolver, sink: EventSink) async -> Op? {
        func confine(_ raw: String) async -> String? {
            do {
                return try resolver.resolve(raw)
            } catch let e as VisionPathError {
                await sink.emit(.error(code: e.code, message: e.description, source: "file"))
                return nil
            } catch {
                await sink.emit(.error(code: "path_denied", message: "\(error)", source: "file"))
                return nil
            }
        }
        switch op {
        case let .analyzeFile(path):
            guard let safe = await confine(path) else { return nil }
            return .analyzeFile(path: safe)
        case let .watchStart(.file(path)):
            guard let safe = await confine(path) else { return nil }
            return .watchStart(source: .file(path: safe))
        default:
            return op
        }
    }

    // -----------------------------------------------------------------------
    // Mode 2 — `vision analyze <imagepath>` (headless single-image CLI).
    // -----------------------------------------------------------------------

    /// Run `vision analyze <imagepath>`: decode the image, run the built-in
    /// Vision detectors via `VisionEngine`, and print a structured JSON result.
    /// Returns the process exit code (0 = ok, non-zero = usage/decode failure).
    /// On-device + offline; no token/socket needed.
    static func runAnalyzeImageCLI(args: [String]) -> Int32 {
        guard let path = args.first, !path.isEmpty else {
            FileHandle.standardError.write(Data("usage: vision analyze <imagepath>\n".utf8))
            return 2
        }
        guard let image = VisionEngine.loadCGImage(path: path) else {
            FileHandle.standardError.write(Data("vision: could not decode image at \(path)\n".utf8))
            return 3
        }
        let engine = VisionEngine()
        let detections = engine.analyze(image: image, detectors: .all, minConfidence: 0.0)
        // Reuse the FROZEN telemetry encoder so the CLI output matches the wire
        // detection shape exactly (topic / by_kind / detections[...]).
        let event = VisionEvent.detections(
            frameIndex: 0, timestamp: 0,
            source: CaptureSource.file(path: path).tag, detections: detections)
        var data = event.encodeData()
        data["compute_unit"] = VisionEngine.computeUnitTag
        data["image_width"] = image.width
        data["image_height"] = image.height
        guard printJSON(data) else {
            FileHandle.standardError.write(Data("vision: failed to encode result\n".utf8))
            return 4
        }
        return 0
    }

    // -----------------------------------------------------------------------
    // Mode 2b — `vision ocr <imagepath>` (headless OCR CLI).
    // -----------------------------------------------------------------------

    /// Run `vision ocr <imagepath>`: decode the image, run ONLY the built-in
    /// VNRecognizeTextRequest via `VisionEngine.recognizeText`, structure the
    /// recognized blocks (reading order + control candidates), and print the
    /// vision.screen readout as JSON. This is the headless OCR proof — the REAL
    /// Vision text recognizer over an in-memory/decoded image with NO camera/
    /// screen/TCC/socket. Reads glyph TEXT, never an identity. Returns the exit
    /// code (0 = ok, non-zero = usage/decode failure).
    static func runOCRImageCLI(args: [String]) -> Int32 {
        guard let path = args.first, !path.isEmpty else {
            FileHandle.standardError.write(Data("usage: vision ocr <imagepath>\n".utf8))
            return 2
        }
        guard let image = VisionEngine.loadCGImage(path: path) else {
            FileHandle.standardError.write(Data("vision: could not decode image at \(path)\n".utf8))
            return 3
        }
        let engine = VisionEngine()
        let dets = engine.recognizeText(image: image, minConfidence: 0.0)
        let readout = ScreenStructurer.structure(dets)
        // Reuse the FROZEN telemetry encoder so the CLI output matches the wire
        // vision.screen shape exactly (text / blocks / controls).
        let event = VisionEvent.screen(
            frameIndex: 0, timestamp: 0,
            source: CaptureSource.file(path: path).tag,
            readout: readout, located: nil, query: nil, meta: .screen)
        var data = event.encodeData()
        data["compute_unit"] = VisionEngine.computeUnitTag
        data["image_width"] = image.width
        data["image_height"] = image.height
        guard printJSON(data) else {
            FileHandle.standardError.write(Data("vision: failed to encode result\n".utf8))
            return 4
        }
        return 0
    }

    // -----------------------------------------------------------------------
    // Mode 2d — `vision handwriting <imagepath>` (#28 headless handwriting CLI).
    // -----------------------------------------------------------------------

    /// Run `vision handwriting <imagepath>`: decode the image, run ONLY the
    /// handwriting recognizer (VNRecognizeTextRequest, .accurate + language
    /// correction — the config best for handwriting/whiteboard) via
    /// `VisionEngine.recognizeHandwriting`, structure the recognized blocks, and
    /// print a vision.screen readout (tagged read_kind=handwriting) as JSON. The
    /// headless #28 proof — the REAL recognizer over an in-memory/decoded image
    /// with NO camera/screen/TCC/socket. Reads glyph TEXT, never an identity.
    /// Honest: recognition QUALITY is device/Vision-model-dependent.
    static func runHandwritingImageCLI(args: [String]) -> Int32 {
        guard let path = args.first, !path.isEmpty else {
            FileHandle.standardError.write(Data("usage: vision handwriting <imagepath>\n".utf8))
            return 2
        }
        guard let image = VisionEngine.loadCGImage(path: path) else {
            FileHandle.standardError.write(Data("vision: could not decode image at \(path)\n".utf8))
            return 3
        }
        let engine = VisionEngine()
        let dets = engine.recognizeHandwriting(image: image, minConfidence: 0.0)
        let readout = ScreenStructurer.structure(dets)
        let event = VisionEvent.screen(
            frameIndex: 0, timestamp: 0,
            source: CaptureSource.file(path: path).tag,
            readout: readout, located: nil, query: nil,
            meta: ScreenReadMeta(kind: .handwriting))
        var data = event.encodeData()
        data["compute_unit"] = VisionEngine.computeUnitTag
        data["image_width"] = image.width
        data["image_height"] = image.height
        guard printJSON(data) else {
            FileHandle.standardError.write(Data("vision: failed to encode result\n".utf8))
            return 4
        }
        return 0
    }

    // -----------------------------------------------------------------------
    // Mode 2e — `vision scan <imagepath>` (#29 headless document-scanner CLI).
    // -----------------------------------------------------------------------

    /// Run `vision scan <imagepath>`: decode the image, run the DOCUMENT SCANNER
    /// (VNDetectDocumentSegmentationRequest -> CIPerspectiveCorrection ->
    /// VNRecognizeTextRequest) via `VisionEngine.scanDocument`, structure the
    /// recognized text off the corrected page, and print a vision.screen readout
    /// (tagged read_kind=document, carrying the HONEST document_detected bool) as
    /// JSON. The headless #29 proof — the REAL segmentation + correction + OCR over
    /// an in-memory/decoded image with NO camera/screen/TCC/socket. When no
    /// document is found the readout is honestly empty (document_detected=false),
    /// never a fabricated page. Reads glyph TEXT, never an identity.
    static func runScanDocumentCLI(args: [String]) -> Int32 {
        guard let path = args.first, !path.isEmpty else {
            FileHandle.standardError.write(Data("usage: vision scan <imagepath>\n".utf8))
            return 2
        }
        guard let image = VisionEngine.loadCGImage(path: path) else {
            FileHandle.standardError.write(Data("vision: could not decode image at \(path)\n".utf8))
            return 3
        }
        let engine = VisionEngine()
        let scan = engine.scanDocument(image: image, minConfidence: 0.0)
        let readout = ScreenStructurer.structure(scan.lines)
        let event = VisionEvent.screen(
            frameIndex: 0, timestamp: 0,
            source: CaptureSource.file(path: path).tag,
            readout: readout, located: nil, query: nil,
            meta: ScreenReadMeta(kind: .document, documentDetected: scan.documentDetected))
        var data = event.encodeData()
        data["compute_unit"] = VisionEngine.computeUnitTag
        data["image_width"] = image.width
        data["image_height"] = image.height
        guard printJSON(data) else {
            FileHandle.standardError.write(Data("vision: failed to encode result\n".utf8))
            return 4
        }
        return 0
    }

    // -----------------------------------------------------------------------
    // Mode 2c — `vision classify-sound <audiopath>` (headless Sound Analysis CLI).
    // -----------------------------------------------------------------------

    /// Run `vision classify-sound <audiopath>`: decode the audio clip, run ONLY
    /// the built-in SNClassifySoundRequest (the ~300-class version1) via
    /// `SoundEngine`, and print the vision.sound readout as JSON. This is the
    /// headless Sound Analysis proof — the REAL classifier over a decoded/in-memory
    /// audio clip with NO microphone, NO TCC, NO socket, NO continuous capture.
    /// ONLY the derived sound-class LABELS are printed; the AUDIO never leaves the
    /// device. DISTINCT from STT (speech): no transcript is produced. Returns the
    /// exit code (0 = ok, non-zero = usage/decode/empty failure).
    static func runClassifySoundCLI(args: [String]) -> Int32 {
        guard let path = args.first, !path.isEmpty else {
            FileHandle.standardError.write(Data("usage: vision classify-sound <audiopath>\n".utf8))
            return 2
        }
        let engine = SoundEngine()
        let classes = engine.classify(audioClipPath: path, minConfidence: 0.0)
        guard !classes.isEmpty else {
            // Honest: a missing/corrupt clip, or a clip too short for the ~3s
            // classifier window, produces NO labels — never fabricate one.
            FileHandle.standardError.write(Data(
                "vision: no sound classes for \(path) (missing/corrupt, or shorter than the ~3s window)\n".utf8))
            return 3
        }
        // Reuse the FROZEN telemetry encoder so the CLI output matches the wire
        // vision.sound shape exactly (classes / classifier / compute_unit).
        let event = VisionEvent.sound(
            timestamp: 0, source: "file", classes: classes,
            classifier: SoundEngine.classifierTag, computeUnit: SoundEngine.computeUnitTag)
        guard printJSON(event.encodeData()) else {
            FileHandle.standardError.write(Data("vision: failed to encode result\n".utf8))
            return 4
        }
        return 0
    }

    // -----------------------------------------------------------------------
    // Mode 3 — `vision analyze-video <file>` (headless video CLI).
    // -----------------------------------------------------------------------

    /// Run `vision analyze-video <file>`: confine the path to videos/input,
    /// decode it through the REAL FileSource (CaptureSourceFactory.make), run the
    /// built-in Vision detectors per decoded frame, and print a per-run summary
    /// including REAL measured per-frame inference latency. .file is the ONLY
    /// capture path verifiable headlessly (no camera/screen/TCC). What is VERIFIED
    /// here: file decode + per-frame detection + measured inference ms. What is
    /// NOT (device-gated, never claimed): camera/screen capture, ANE residency,
    /// and real-time capture fps. Returns the process exit code.
    static func runAnalyzeVideoCLI(args: [String]) async -> Int32 {
        guard let raw = args.first, !raw.isEmpty else {
            FileHandle.standardError.write(Data("usage: vision analyze-video <file>\n".utf8))
            return 2
        }
        // Confine to videos/input under the cwd-rooted project, exactly as the
        // socket-served analyze.file op does. An absolute or escaping path is a
        // clean error, never an open.
        let resolver = VideoPathResolver(projectRoot: FileManager.default.currentDirectoryPath)
        let path: String
        do {
            path = try resolver.resolve(raw)
        } catch let e as VisionPathError {
            FileHandle.standardError.write(Data("vision analyze-video: \(e.description)\n".utf8))
            return 2
        } catch {
            FileHandle.standardError.write(Data("vision analyze-video: \(error)\n".utf8))
            return 2
        }

        // Drive the REAL file FrameSource through the engine, summarizing per
        // decoded frame. This is the same factory the production socket path uses.
        let source: FrameSource = CaptureSourceFactory.make(for: .file(path: path))
        let auth = await source.authorization()
        guard auth == .notApplicable || auth == .authorized else {
            FileHandle.standardError.write(Data("vision analyze-video: not authorized (\(auth.rawValue))\n".utf8))
            return 1
        }
        let engine = VisionEngine()
        var frameCount: UInt64 = 0
        var totalDetections = 0
        var inferenceMsSamples: [Double] = []
        for await frame in source.frames() {
            // MEASURED: detectTimed brackets the real VNImageRequestHandler.perform
            // with a monotonic timer, so inferenceMs is a genuine per-frame number.
            let (dets, inferenceMs) = engine.detectTimed(in: frame, detectors: .all, minConfidence: 0.0)
            totalDetections += dets.count
            if inferenceMs > 0 { inferenceMsSamples.append(inferenceMs) }
            frameCount += 1
        }
        // vision.perf carries REAL measured inference latency (p50/p95 ms) over
        // the decoded frames. We deliberately OMIT real-time fps: that is the
        // live-camera capture rate, which is device-gated and NOT measured on the
        // headless file path. The per-frame INFERENCE latency below IS measured.
        var perf: [String: Any] = [
            "topic": "vision.perf",
            "frames": frameCount,
            "detections_total": totalDetections,
            "source": "file",
            "compute_unit": VisionEngine.computeUnitTag,
        ]
        if !inferenceMsSamples.isEmpty {
            perf["inference_ms_p50"] = Pipeline.percentile(inferenceMsSamples, 0.50)
            perf["inference_ms_p95"] = Pipeline.percentile(inferenceMsSamples, 0.95)
        }
        _ = printJSON(perf)
        return 0
    }

    // -----------------------------------------------------------------------
    // helpers
    // -----------------------------------------------------------------------

    static func printUsage() {
        let usage = """
        vision — JARVIS on-device computer-vision micro-app (defensive, offline).

        Run modes:
          vision                          socket-served runtime (launched by jarvisd;
                                          needs JARVIS_APP_TOKEN / _SOCKET / _NAME)
          vision analyze <imagepath>      headless: detect in one image, print JSON
          vision ocr <imagepath>          headless: read TEXT (OCR) in one image,
                                          print the vision.screen readout as JSON
          vision handwriting <imagepath>  headless: read HANDWRITING/whiteboard text
                                          (.accurate + language correction) in one
                                          image, print the vision.screen readout
          vision scan <imagepath>         headless: DOCUMENT SCAN (detect page quad
                                          -> perspective-correct -> OCR) one image,
                                          print the vision.screen readout (honest
                                          document_detected; never a fabricated page)
          vision classify-sound <audiopath>
                                          headless: classify ONE audio clip with the
                                          built-in Sound Analysis ~300-class
                                          classifier, print the vision.sound readout
                                          as JSON (LABELS only; audio never leaves)
          vision analyze-video <file>     headless: detect per frame of a confined
                                          video under apps/vision/videos/input
          vision help                     this message

        Defensive: on-device only, no upload, no identity recognition. Camera/
        screen require macOS TCC user consent at runtime (not granted by argv).
        """
        FileHandle.standardError.write(Data((usage + "\n").utf8))
    }

    /// Serialize a payload dict to one JSON line on stdout (sorted keys for
    /// deterministic output). Never carries pixels — counts/boxes/labels only.
    /// Returns false on a serialization failure (the caller maps it to an exit).
    @discardableResult
    static func printJSON(_ payload: [String: Any]) -> Bool {
        guard JSONSerialization.isValidJSONObject(payload),
              let data = try? JSONSerialization.data(withJSONObject: payload, options: [.sortedKeys]),
              let s = String(data: data, encoding: .utf8)
        else { return false }
        FileHandle.standardOutput.write(Data((s + "\n").utf8))
        return true
    }
}
