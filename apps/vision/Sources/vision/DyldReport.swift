// DyldReport.swift — READ-ONLY dyld module self-report (docs/INTROSPECT.md).
//
// The Swift analogue of apps/_sdk/dyld_report.py: enumerate THIS process's loaded
// dyld images (path + LC_UUID) via the public _dyld_* C API and hand them to the
// daemon over the existing token-stamped socket as a VisionEvent.modules line. The
// daemon (daemon/src/introspect.rs) seeds a trust-on-first-use baseline and flags
// any module a later report adds (injection / unexpected dlopen).
//
// COOPERATIVE + READ-ONLY: in-process enumeration only — no entitlement, no
// task_for_pid, no ptrace. It reports the app's OWN image list; it is reliable
// against injection into an otherwise-honest app, NOT a defense against an app
// that lies about itself (bounded by the sandbox + per-launch token). Never
// raises: any failure yields an empty/partial list.

import Foundation
#if canImport(Darwin)
import Darwin
import MachO
import os  // OSAllocatedUnfairLock
#endif

#if canImport(Darwin)
// Thread-safe "a new image was loaded" flag, set by the dyld add-image callback
// (which runs on whatever thread performs the dlopen). The callback is a plain
// C-callable function (no captures); it only flips the flag — it never touches
// the socket — so there is no cross-thread emit or actor reentrancy.
private let dyldChanged = OSAllocatedUnfairLock<Bool>(initialState: false)
private nonisolated(unsafe) var dyldWatching = false

private func dyldOnAddImage(_ header: UnsafePointer<mach_header>?, _ slide: Int) {
    dyldChanged.withLock { $0 = true }
}
#endif

/// One loaded module: its (logical) dyld image path and, when LC_UUID parsed, the
/// build UUID. On Apple Silicon most system dylibs live in the shared cache with
/// no standalone file, so the path is a logical name — the UUID is the tamper-
/// resistant identity when present.
public struct DyldModule: Sendable, Equatable {
    public let path: String
    public let uuid: String?

    public init(path: String, uuid: String?) {
        self.path = path
        self.uuid = uuid
    }
}

public enum DyldReport {
    /// Cap on images enumerated (bounds a pathological process; mirrors the
    /// daemon's MAX_MODULES and the Python stub).
    private static let maxImages = 8192

    /// Register a dyld add-image callback so a LATER dlopen sets a "changed" flag.
    /// Idempotent, macOS-only. Registration fires the callback for every already-
    /// loaded image, so the flag is cleared right after — only future dlopens leave
    /// it set. This matters for vision specifically: Vision / ScreenCaptureKit /
    /// CoreML are lazy-loaded when capture STARTS, after the one-shot startup report.
    public static func watch() {
        #if canImport(Darwin)
        guard !dyldWatching else { return }
        dyldWatching = true
        _dyld_register_func_for_add_image(dyldOnAddImage)  // fires for existing images
        dyldChanged.withLock { $0 = false }                // discard the initial bulk
        #endif
    }

    /// True (and resets the flag) iff an image was loaded since the last check or
    /// since watch(). False if not watching or nothing changed.
    public static func consumeChanged() -> Bool {
        #if canImport(Darwin)
        return dyldChanged.withLock { c in
            let was = c
            c = false
            return was
        }
        #else
        return false
        #endif
    }

    /// Every loaded dyld image as a DyldModule. Empty on non-Darwin.
    public static func collectLoadedModules() -> [DyldModule] {
        #if canImport(Darwin)
        var out: [DyldModule] = []
        let count = min(Int(_dyld_image_count()), maxImages)
        var i = 0
        while i < count {
            defer { i += 1 }
            guard let namePtr = _dyld_get_image_name(UInt32(i)) else { continue }
            let path = String(cString: namePtr)
            let uuid = imageUUID(_dyld_get_image_header(UInt32(i)))
            out.append(DyldModule(path: path, uuid: uuid))
        }
        return out
        #else
        return []
        #endif
    }

    #if canImport(Darwin)
    /// Parse LC_UUID out of a 64-bit Mach-O header (the app's OWN in-process image
    /// header), or nil if absent / not 64-bit / malformed.
    private static func imageUUID(_ header: UnsafePointer<mach_header>?) -> String? {
        guard let header else { return nil }
        let raw = UnsafeRawPointer(header)
        let mh = raw.load(as: mach_header_64.self)
        guard mh.magic == MH_MAGIC_64 else { return nil }  // 64-bit only (arm64/x86_64)

        var cmd = raw.advanced(by: MemoryLayout<mach_header_64>.size)
        let minCmd = MemoryLayout<load_command>.size
        var remaining = Int(mh.ncmds)
        while remaining > 0 {
            let lc = cmd.load(as: load_command.self)
            guard Int(lc.cmdsize) >= minCmd else { break }  // malformed -> stop
            if lc.cmd == UInt32(LC_UUID) {
                let uc = cmd.load(as: uuid_command.self)
                return UUID(uuid: uc.uuid).uuidString
            }
            cmd = cmd.advanced(by: Int(lc.cmdsize))
            remaining -= 1
        }
        return nil
    }
    #endif
}
