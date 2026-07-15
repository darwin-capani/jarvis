//! Mark-Forge binary entry — the daemon-launched micro-app process.
//!
//! Runtime contract (`daemon/src/apps.rs`, runtime = "binary", mirror of
//! silicon-canvas): darwind execs this binary directly under `sandbox-exec`,
//! handing it the socket + token via the ENVIRONMENT (NEVER argv — argv is
//! world-readable via `ps`):
//!   - `DARWIN_APP_SOCKET` — abs path of the per-app Unix socket
//!   - `DARWIN_APP_TOKEN`  — the capability token to stamp on every line
//!   - `DARWIN_APP_NAME`   — "mark-forge"
//!
//! Like silicon-canvas / global-scan, it REFUSES to run standalone (no
//! socket/token): this app only runs under the daemon, so an accidental direct
//! invocation exits with code 2 rather than binding anything. When the env is
//! present it connects to the daemon's socket and hands control to
//! [`mark_forge::ipc::run`], the JSONL op/telemetry loop.
//!
//! HARD SAFETY (never violated here): this binary binds NO listener (it CONNECTS
//! to the daemon's socket), opens NO window, touches NO GPU (the engine is pure
//! CPU/f64 — the HUD renders the bodies), and plays NO audio.

use std::process::ExitCode;

use mark_forge::error::MarkForgeError;
use mark_forge::ipc::{self, AppEnv};

fn main() -> ExitCode {
    // Read the launch env exactly as apps.rs establishes it. Absent the socket
    // or token, we are not running under darwind — refuse with exit code 2,
    // binding nothing.
    let env = match AppEnv::from_env() {
        Ok(env) => env,
        Err(MarkForgeError::Unauthorized) => {
            eprintln!(
                "mark-forge: DARWIN_APP_SOCKET and DARWIN_APP_TOKEN must be set \
                 (this app runs under darwind, not standalone)"
            );
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("mark-forge: cannot read launch environment: {e}");
            return ExitCode::from(2);
        }
    };

    // Connect to the daemon's per-app socket and run the JSONL op/telemetry loop.
    // ipc::run connects (never binds), reads daemon control verbs + ops, and
    // writes token-stamped telemetry back. It returns Ok on a clean `stop` /
    // socket close. No GPU/window/audio is touched on this path.
    match ipc::run(env) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mark-forge: {e}");
            ExitCode::FAILURE
        }
    }
}
