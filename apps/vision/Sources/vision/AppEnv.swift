// AppEnv.swift — the launch environment the daemon hands every micro-app
// (FROZEN; module agents build against it, must not change it).
//
// Contract (daemon/src/apps.rs, ~line 1286): the host passes the per-app
// socket + capability token via the launch ENV ONLY (never argv — argv is
// world-readable via ps). The three keys are:
//     DARWIN_APP_TOKEN   hex HMAC-SHA256 capability token; stamped on EVERY
//                        app->host line so the host can verify it.
//     DARWIN_APP_SOCKET  absolute path to this app's per-app Unix socket
//                        (JSONL); the app connect()s to it.
//     DARWIN_APP_NAME    the app's name ("vision"); matches the manifest +
//                        directory + telemetry "name".
//
// Interim TCC-declaration channel (until the daemon schema gains camera/screen
// — see manifest.toml): DARWIN_VISION_CAMERA / DARWIN_VISION_SCREEN, parsed as
// booleans, default false. These DECLARE intent only; macOS TCC remains the
// real gate and is requested on-device at first capture.

import Foundation

/// The validated launch environment for the Vision micro-app.
public struct AppEnv: Sendable, Equatable {
    /// Hex capability token to stamp on every app->host line.
    public let token: String
    /// Absolute path to this app's per-app Unix socket.
    public let socketPath: String
    /// The app's name (expected: "vision").
    public let name: String

    /// Interim declared-capability flags (TCC is still the real gate).
    public let cameraDeclared: Bool
    public let screenDeclared: Bool

    public init(token: String, socketPath: String, name: String,
                cameraDeclared: Bool = false, screenDeclared: Bool = false) {
        self.token = token
        self.socketPath = socketPath
        self.name = name
        self.cameraDeclared = cameraDeclared
        self.screenDeclared = screenDeclared
    }

    /// Env var keys — single source of truth (the daemon writes these exact
    /// names; do not rename without changing apps.rs).
    public enum Key {
        public static let token  = "DARWIN_APP_TOKEN"
        public static let socket = "DARWIN_APP_SOCKET"
        public static let name   = "DARWIN_APP_NAME"
        public static let camera = "DARWIN_VISION_CAMERA"
        public static let screen = "DARWIN_VISION_SCREEN"
    }

    /// Why the env was unusable — surfaced as a clean exit, not a crash.
    public enum EnvError: Error, Equatable, CustomStringConvertible {
        case missing(String)   // a required key was absent or empty
        public var description: String {
            switch self {
            case .missing(let k): return "required launch env var \(k) is missing or empty"
            }
        }
    }

    /// Load from a key->value dictionary (the real loader passes
    /// ProcessInfo.processInfo.environment; tests pass a literal dict, so the
    /// parsing is exercised with NO process env mutation).
    public static func load(from env: [String: String]) throws -> AppEnv {
        func required(_ key: String) throws -> String {
            guard let v = env[key], !v.isEmpty else { throw EnvError.missing(key) }
            return v
        }
        let token  = try required(Key.token)
        let socket = try required(Key.socket)
        let name   = try required(Key.name)
        return AppEnv(
            token: token,
            socketPath: socket,
            name: name,
            cameraDeclared: parseBool(env[Key.camera]),
            screenDeclared: parseBool(env[Key.screen])
        )
    }

    /// Load from the live process environment.
    public static func loadFromProcess() throws -> AppEnv {
        try load(from: ProcessInfo.processInfo.environment)
    }

    /// Lenient boolean parse: "1"/"true"/"yes"/"on" (any case) -> true; else false.
    static func parseBool(_ raw: String?) -> Bool {
        guard let raw = raw?.trimmingCharacters(in: .whitespaces).lowercased() else { return false }
        return raw == "1" || raw == "true" || raw == "yes" || raw == "on"
    }
}
