// swift-tools-version: 6.0
// Vision — DARWIN on-device computer-vision micro-app.
//
// Defensive, on-device ONLY. Uses Apple's BUILT-IN Vision/Core ML requests
// (no external model download, fully offline). Inference runs headlessly on
// ANE/GPU; camera/screen capture require macOS TCC user consent at runtime
// (NOT grantable by the SBPL profile — the manifest only DECLARES the need).
//
// Target platform: macOS 14+ (ScreenCaptureKit SCStream + modern Vision).
// swiftc 6.3.2 (arm64) verified present this session.

import PackageDescription

let package = Package(
    name: "vision",
    platforms: [
        // macOS 14 (Sonoma) is the floor: ScreenCaptureKit's SCStream/
        // SCContentFilter and the current Vision request set are all available,
        // and the daemon hosts on macOS 26 (verified in env).
        .macOS(.v14)
    ],
    products: [
        // The micro-app binary the daemon launches (runtime = "binary").
        .executable(name: "vision", targets: ["vision"])
    ],
    targets: [
        // Single executable target holding ALL modules (inference, capture,
        // pipeline, ipc, main) as disjoint Source files so parallel module
        // agents fill non-overlapping files. System frameworks are linked
        // implicitly by `import` on Apple platforms; we also declare them in
        // linkerSettings so a clean checkout links deterministically.
        .executableTarget(
            name: "vision",
            path: "Sources/vision",
            linkerSettings: [
                .linkedFramework("Vision"),
                .linkedFramework("CoreML"),
                .linkedFramework("AVFoundation"),
                // SoundAnalysis: Apple's built-in SNClassifySoundRequest (the
                // ~300-class SNClassifierIdentifier.version1) — the AUDIO analog
                // of VNClassifyImage. Built-in, on-device, offline (no model
                // download). Slots in additively like the Vision requests: an
                // audio clip/buffer -> [sound label, confidence].
                .linkedFramework("SoundAnalysis"),
                .linkedFramework("ScreenCaptureKit"),
                .linkedFramework("CoreImage"),
                .linkedFramework("CoreGraphics"),
                .linkedFramework("CoreVideo"),
                .linkedFramework("ImageIO"),
                .linkedFramework("Foundation"),
                // EMBED Info.plist into the binary's __TEXT,__info_plist section so
                // macOS reads CFBundleDisplayName="D.A.R.W.I.N." for this binary's
                // Camera/Screen TCC prompts (it is the real capturer) plus the
                // Camera/Mic usage strings. SwiftPM resolves this linker path
                // relative to the PACKAGE ROOT, so it works with both `swift build`
                // run from apps/vision and the installer's `--package-path apps/vision`
                // (verified: the __info_plist section is present either way).
                .unsafeFlags([
                    "-Xlinker", "-sectcreate",
                    "-Xlinker", "__TEXT",
                    "-Xlinker", "__info_plist",
                    "-Xlinker", "Info.plist",
                ])
            ]
        ),
        // XCTest target. Tests drive the PURE logic (shared types, Op
        // decoding, telemetry encoding, AppEnv parsing, classify-from-
        // synthesized-CGImage) with NO camera, NO screen, NO TCC.
        .testTarget(
            name: "visionTests",
            dependencies: ["vision"],
            path: "Tests/visionTests"
        )
    ]
)
