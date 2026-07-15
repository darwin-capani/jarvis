//! Crate error type (mirror of silicon-canvas `error.rs` discipline).
//!
//! `thiserror` gives each variant a `Display` string and `#[from]` conversions so
//! `?` flows naturally. `anyhow` is used only at the binary top level (`main.rs`);
//! library code returns the typed [`MarkForgeError`] so callers can match.
//!
//! This module is the CONTRACT. Downstream agents return [`MarkForgeError`] /
//! `Result<T>` and may match its variants; they must NOT change the type.

use thiserror::Error;

/// The crate-wide error type. Every library function that can fail returns
/// `Result<T, MarkForgeError>` (aliased [`Result`]).
#[derive(Debug, Error)]
pub enum MarkForgeError {
    /// Filesystem / I/O failure (connecting the socket, the optional trace dump).
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// An inbound IPC line was not valid JSON / not a recognized op or control
    /// verb. Classified (never panicked) so the socket loop drops the line and
    /// keeps serving (SPEC §7).
    #[error("malformed IPC message: {0}")]
    Protocol(String),

    /// JSON (de)serialization of an op / telemetry payload failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// An op referenced a body id that does not exist in the current world.
    #[error("no such body: {0}")]
    NoSuchBody(u32),

    /// A spawn op carried a non-finite or otherwise invalid transform/shape
    /// parameter (NaN/inf position, negative radius). Rejected before the body
    /// enters the world so the integrator never sees a poisoned value (SPEC §1
    /// determinism — no NaN propagation).
    #[error("invalid spawn parameters: {0}")]
    InvalidSpawn(String),

    /// The capability token / launch env was missing (`DARWIN_APP_SOCKET` or
    /// `DARWIN_APP_TOKEN` absent) — the app only runs under the daemon. The
    /// binary's `main` maps this to exit code 2.
    #[error("unauthorized: DARWIN_APP_SOCKET and DARWIN_APP_TOKEN must be set (this app runs under darwind, not standalone)")]
    Unauthorized,
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, MarkForgeError>;

impl MarkForgeError {
    /// Build a [`MarkForgeError::Protocol`] from any displayable message.
    pub fn protocol(message: impl Into<String>) -> Self {
        MarkForgeError::Protocol(message.into())
    }

    /// Build a [`MarkForgeError::InvalidSpawn`] from any displayable message.
    pub fn invalid_spawn(message: impl Into<String>) -> Self {
        MarkForgeError::InvalidSpawn(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_constructor_carries_message() {
        let e = MarkForgeError::protocol("bad line");
        assert!(matches!(e, MarkForgeError::Protocol(_)));
        assert!(e.to_string().contains("bad line"));
    }

    #[test]
    fn io_error_converts() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        let e: MarkForgeError = io.into();
        assert!(matches!(e, MarkForgeError::Io(_)));
    }

    #[test]
    fn unauthorized_message_mentions_env_vars() {
        let e = MarkForgeError::Unauthorized;
        assert!(e.to_string().contains("DARWIN_APP_SOCKET"));
        assert!(e.to_string().contains("DARWIN_APP_TOKEN"));
    }
}
