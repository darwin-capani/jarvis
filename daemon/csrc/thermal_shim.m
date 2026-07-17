// thermal_shim.m — a thin Objective-C shim over Foundation's
// `NSProcessInfo.thermalState`, mirroring the es_shim precedent (a small
// system call compiled against Apple's REAL <Foundation/Foundation.h>, so the
// enum names are COMPILER-VERIFIED, not hand-transcribed). It reads ONE
// process-wide OS signal and returns a flat int to Rust.
//
// READ-ONLY + UNPRIVILEGED: `thermalState` is a public, process-scoped signal
// that every process can read — it needs NO entitlement, NO root, and NO
// powermetrics/sudo. It reports the OS's own thermal-pressure ladder
// (Nominal/Fair/Serious/Critical) that macOS already computes; this shim only
// observes it. It takes no action and touches no actuator.
//
// Compiled UNCONDITIONALLY on macOS by build.rs (Foundation is always present
// and links freely), so `cargo build`/`cargo test` link it on any Mac. The
// PURE mapping of the returned int -> ThermalState lives in Rust (power.rs
// `map_thermal`) and is unit-tested there; this live read is the device-gated
// runner and is never exercised under test.

#import <Foundation/Foundation.h>

// Returns the current ProcessInfo.thermalState as a flat, stable int:
//   0 = Nominal, 1 = Fair, 2 = Serious, 3 = Critical, -1 = unknown/unreadable.
// The Rust side maps an unknown (-1) to Nominal so the policy never throttles on
// a guess. No object is retained beyond the shared, autoreleased singleton, so
// there is no ownership to manage (a scalar read).
int darwin_thermal_state(void) {
    NSProcessInfoThermalState st = [[NSProcessInfo processInfo] thermalState];
    switch (st) {
        case NSProcessInfoThermalStateNominal:  return 0;
        case NSProcessInfoThermalStateFair:     return 1;
        case NSProcessInfoThermalStateSerious:  return 2;
        case NSProcessInfoThermalStateCritical: return 3;
        default:                                return -1;
    }
}
