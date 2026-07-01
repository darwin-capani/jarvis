//! #36 PLUGIN SDK — the formalized capability-module CONTRACT for micro-apps.
//!
//! A plugin is a sandboxed micro-app (docs/SANDBOX.md): a separate process under
//! a default-deny seatbelt profile, holding a per-launch HMAC capability token,
//! reachable only over its own JSONL socket. This module adds the contract that
//! lets a plugin DECLARE what it answers (`[intents]`) and what it exposes
//! (`[tools]`, each with the capability scopes it requests), plus:
//!
//!   (a) a PURE manifest VALIDATOR — [`validate_manifest`] — that checks the
//!       schema (required fields, well-formed intent/tool names), that every
//!       requested capability scope is within the ALLOWED set, and that no
//!       requested scope claims a privilege the plugin's own `[permissions]`
//!       block (and hence the SBPL profile) does not actually grant. A malformed
//!       or OVER-PRIVILEGED manifest is rejected with a precise error.
//!   (b) the register-on-launch HANDSHAKE — [`register_plugin`] — the plugin
//!       presents its manifest + its capability token; the daemon RE-VALIDATES
//!       the manifest and VERIFIES the token (the SAME HMAC/nonce machinery as
//!       the per-app relay and the generate proxy — no new crypto) before
//!       scoping the plugin's declared intents/tools onto the router. A
//!       forged/cross-app/stale token, or an invalid manifest, fails the
//!       handshake — the plugin is not admitted.
//!   (c) capability-token SCOPING per SANDBOX.md: the token already binds
//!       `name || canonical(permissions) || nonce`, so a plugin that widens its
//!       permissions after launch fails verification. The validator additionally
//!       ensures the declared TOOL scopes never exceed the permission set the
//!       token is bound to — the token is the authority, the manifest is its
//!       auditable description.
//!
//! WHAT THE SANDBOX STILL ENFORCES (unchanged): a plugin can NOT escape the
//! default-deny SBPL profile (the [`crate::apps::generate_sbpl`] derivation is
//! untouched — declaring an intent grants nothing), can NOT request a capability
//! outside the allowed set (this validator rejects it), and any consequential
//! tool it exposes still rides the cross-turn confirmation gate + the
//! armed-by-default `[integrations].allow_consequential` master switch (ON, but a
//! confirmed action still needs a fresh confirm) when invoked.
//!
//! SHIPS ON (full-power default). The LIVE handshake is gated by `[plugin_sdk].enabled`
//! (default true). The validator is PURE and always callable. HERMETIC: the
//! tests validate a good manifest (accepts), a malformed one (precise error), an
//! over-privileged one (rejected), and prove the example plugin validates — NO
//! real spawn, NO socket.

use crate::apps::{verify_token_with_key, AppManifest, PermissionsSection, ToolDecl};

/// The COMPLETE set of capability scopes a plugin tool may request. A scope
/// outside this set is rejected by [`validate_manifest`] — this is the "no
/// privilege the sandbox forbids" allowlist (it mirrors the seatbelt-grantable
/// permission dimensions in `PermissionsSection`). Adding a scope here is a
/// deliberate widening of the contract, never an accident.
pub const ALLOWED_SCOPES: &[&str] = &[
    "net",      // outbound network — requires non-empty net_hosts
    "fs_read",  // filesystem read — requires non-empty fs_read
    "fs_write", // filesystem write — requires non-empty fs_write
    "audio",    // daemon audio API — requires audio = true
    "gpu",      // Metal/GPU — requires gpu = true
    "camera",   // TCC-gated camera DECLARATION — requires camera = true
    "screen",   // TCC-gated screen DECLARATION — requires screen = true
    "generate", // the daemon-mediated generate proxy (no extra permission)
];

/// Outcome of a manifest validation: either the parsed+validated manifest, or a
/// precise error string. `Result<AppManifest, String>` keeps the error a plain
/// human-readable message (surfaced to the operator / HUD), never an opaque code.
pub type ValidateResult = Result<AppManifest, String>;

/// Is `name` a well-formed dotted-lowercase identifier (an intent or tool name)?
/// Segments of `[a-z0-9_]+` joined by single dots; non-empty; no leading/trailing
/// dot, no empty segment. Pure + tiny so it is unit-testable. Mirrors the strict
/// discipline of `integrations::is_safe_mcp_server_name` for the flat-id world.
fn is_well_formed_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let segments: Vec<&str> = name.split('.').collect();
    segments.iter().all(|seg| {
        !seg.is_empty()
            && seg
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    })
}

/// Does the plugin's `[permissions]` block actually grant the privilege a tool
/// scope claims? The scope is auditable ONLY if the permission set backs it —
/// e.g. a tool requesting `"net"` while `net_hosts` is empty is OVER-PRIVILEGED
/// (it claims network the sandbox profile would deny). `"generate"` needs no
/// extra permission (the proxy is a separate op-restricted socket). This is the
/// "no privilege the sandbox forbids" cross-check.
fn scope_backed_by_permissions(scope: &str, p: &PermissionsSection) -> Result<(), String> {
    let backed = match scope {
        "net" => !p.net_hosts.is_empty(),
        "fs_read" => !p.fs_read.is_empty(),
        "fs_write" => !p.fs_write.is_empty(),
        "audio" => p.audio,
        "gpu" => p.gpu,
        "camera" => p.camera,
        "screen" => p.screen,
        // The generate proxy is granted via fs_read on the proxy socket, but it
        // is a daemon-mediated capability needing no device/net permission of its
        // own — always allowable to declare.
        "generate" => true,
        // Unknown scope is handled by the ALLOWED_SCOPES check before this.
        _ => true,
    };
    if backed {
        Ok(())
    } else {
        Err(format!(
            "tool scope {scope:?} is over-privileged: the [permissions] block does not grant it \
             (declare the matching permission, or drop the scope)"
        ))
    }
}

/// Validate ONE exposed tool's declaration: non-empty well-formed name, and every
/// requested scope is (1) in [`ALLOWED_SCOPES`] and (2) backed by the permission
/// set. Returns a precise error naming the offending tool/scope.
fn validate_tool(tool: &ToolDecl, perms: &PermissionsSection) -> Result<(), String> {
    if tool.name.trim().is_empty() {
        return Err("a [[tools.exposes]] entry has an empty name".to_string());
    }
    if !is_well_formed_name(&tool.name) {
        return Err(format!(
            "tool name {:?} is malformed (expected dotted lowercase a-z0-9_ segments)",
            tool.name
        ));
    }
    for scope in &tool.scopes {
        if !ALLOWED_SCOPES.contains(&scope.as_str()) {
            return Err(format!(
                "tool {:?} requests unknown capability scope {:?} (allowed: {:?})",
                tool.name, scope, ALLOWED_SCOPES
            ));
        }
        scope_backed_by_permissions(scope, perms)
            .map_err(|e| format!("tool {:?}: {e}", tool.name))?;
    }
    Ok(())
}

/// THE pure manifest validator. Parses `raw` TOML for `dir_name` (which enforces
/// the base [`AppManifest`] contract — name matches the directory, runtime known,
/// non-empty version/entry — via [`AppManifest::parse`]), then validates the #36
/// `[intents]`/`[tools]` contract:
///
///   - every `[intents].provides` name is a well-formed dotted-lowercase id;
///   - every `[[tools.exposes]]` has a well-formed name;
///   - every requested tool scope is within [`ALLOWED_SCOPES`] (no privilege the
///     contract does not define) AND backed by the `[permissions]` block (no
///     privilege the sandbox would deny — the over-privilege check).
///
/// Returns the validated manifest, or a PRECISE error (the operator/HUD reads it
/// verbatim). PURE: no I/O, no spawn — the unit tests drive it with manifest
/// strings.
pub fn validate_manifest(raw: &str, dir_name: &str) -> ValidateResult {
    // Base contract first — this reuses the SAME parse/validate the launcher uses,
    // so an SDK-valid manifest is by construction a launch-valid manifest.
    let manifest = AppManifest::parse(raw, dir_name).map_err(|e| e.to_string())?;

    // [intents] — every provided intent name must be well-formed.
    for intent in &manifest.intents.provides {
        if !is_well_formed_name(intent) {
            return Err(format!(
                "intent name {intent:?} is malformed (expected dotted lowercase a-z0-9_ segments)"
            ));
        }
    }

    // [tools] — every exposed tool, name + scopes.
    for tool in &manifest.tools.exposes {
        validate_tool(tool, &manifest.permissions)?;
    }

    Ok(manifest)
}

/// Outcome of the register-on-launch handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeOutcome {
    /// Manifest valid AND token verified: the plugin is ADMITTED with these
    /// scoped intents (the names the daemon will route to it).
    Admitted { name: String, intents: Vec<String> },
    /// The presented manifest failed validation (precise reason).
    InvalidManifest(String),
    /// The manifest was valid but the capability token failed verification
    /// (forged / tampered / cross-app / stale / missing) — fail-closed.
    Unauthorized,
}

/// The register-on-launch HANDSHAKE (pure over the session key). At launch a
/// plugin presents (1) its manifest TOML, (2) the per-launch capability token
/// it was handed, and (3) the name/permissions/nonce the daemon minted the token
/// against. The daemon:
///
///   1. RE-VALIDATES the manifest with [`validate_manifest`] (defense in depth:
///      the on-disk manifest is checked at discovery, and again here against what
///      the plugin actually presents);
///   2. VERIFIES the token CONSTANT-TIME via the SAME HMAC machinery as the
///      per-app relay ([`verify_token_with_key`]) — a forged/cross-app/stale
///      token fails closed and the plugin is NOT admitted;
///   3. on success, returns the SCOPED intent set the plugin may answer.
///
/// `session_key` / `nonce` are the daemon's launch secrets; the production caller
/// passes the live values, the tests pass fixed ones — so the handshake is proven
/// without a real spawn or socket.
pub fn register_plugin(
    raw_manifest: &str,
    dir_name: &str,
    presented_token: &str,
    session_key: &[u8],
    nonce: &str,
) -> HandshakeOutcome {
    let manifest = match validate_manifest(raw_manifest, dir_name) {
        Ok(m) => m,
        Err(e) => return HandshakeOutcome::InvalidManifest(e),
    };

    // The token is bound to name || canonical(permissions) || nonce — the SAME
    // shape the per-app relay verifies. A plugin that widened its permissions, a
    // token lifted from another app, or a stale (pre-restart) token fails here.
    let verified = verify_token_with_key(
        session_key,
        manifest.name(),
        &manifest.permissions,
        nonce,
        presented_token,
    );
    if !verified {
        return HandshakeOutcome::Unauthorized;
    }

    HandshakeOutcome::Admitted {
        name: manifest.name().to_string(),
        intents: manifest.intents.provides.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::compute_token;

    // A minimal VALID plugin manifest with an intent + a read-only tool whose one
    // scope (generate) is always backed.
    const GOOD: &str = r#"
        [app]
        name        = "example-plugin"
        version     = "0.1.0"
        description = "A minimal example plugin illustrating the #36 capability contract."
        entry       = "apps/example-plugin/main.py"
        runtime     = "python"

        [permissions]
        net_hosts = []
        fs_read   = ["state/ipc/apps/generate.sock"]
        fs_write  = ["state/apps/example-plugin"]

        [intents]
        provides = ["example.status"]

        [[tools.exposes]]
        name = "example.read_status"
        scopes = ["generate"]
        consequential = false
    "#;

    fn good_for(dir: &str) -> String {
        GOOD.replace("example-plugin", dir)
    }

    // -- name well-formedness ------------------------------------------------

    #[test]
    fn well_formed_name_accepts_dotted_lowercase_and_rejects_junk() {
        assert!(is_well_formed_name("fab.status"));
        assert!(is_well_formed_name("example.read_status"));
        assert!(is_well_formed_name("plain"));
        assert!(!is_well_formed_name(""), "empty rejected");
        assert!(!is_well_formed_name(".lead"), "leading dot rejected");
        assert!(!is_well_formed_name("trail."), "trailing dot rejected");
        assert!(!is_well_formed_name("a..b"), "empty segment rejected");
        assert!(!is_well_formed_name("Caps.Here"), "uppercase rejected");
        assert!(!is_well_formed_name("has space"), "space rejected");
        assert!(!is_well_formed_name("semi;colon"), "punctuation rejected");
    }

    // -- (a) validator: accepts a good manifest ------------------------------

    #[test]
    fn validate_accepts_a_good_manifest() {
        let m = validate_manifest(GOOD, "example-plugin").expect("good manifest must validate");
        assert_eq!(m.name(), "example-plugin");
        assert_eq!(m.intents.provides, vec!["example.status".to_string()]);
        assert_eq!(m.tools.exposes.len(), 1);
        assert_eq!(m.tools.exposes[0].name, "example.read_status");
    }

    /// A manifest with NO [intents]/[tools] (an existing app-style manifest) still
    /// validates — the contract block is optional and backward compatible.
    #[test]
    fn validate_accepts_a_manifest_without_the_contract_block() {
        let raw = r#"
            [app]
            name        = "plain-app"
            version     = "0.1.0"
            description = "An app with no declared intents or tools."
            entry       = "plain"
            runtime     = "binary"
        "#;
        let m = validate_manifest(raw, "plain-app").expect("must validate");
        assert!(m.intents.provides.is_empty());
        assert!(m.tools.exposes.is_empty());
    }

    // -- (a) validator: rejects malformed ------------------------------------

    #[test]
    fn validate_rejects_a_malformed_manifest_with_a_precise_error() {
        // name != directory — the base contract violation, surfaced precisely.
        let err = validate_manifest(GOOD, "wrong-dir").unwrap_err();
        assert!(
            err.contains("must match its directory name"),
            "precise base error, got: {err}"
        );

        // Not valid TOML at all.
        let err = validate_manifest("this is not toml = =", "x").unwrap_err();
        assert!(!err.is_empty(), "a parse failure yields a non-empty error");

        // A malformed intent name.
        let raw = good_for("p").replace(r#"provides = ["example.status"]"#, r#"provides = ["Bad Name"]"#);
        let err = validate_manifest(&raw, "p").unwrap_err();
        assert!(err.contains("intent name") && err.contains("malformed"), "got: {err}");

        // A malformed tool name.
        let raw = good_for("p").replace(r#"name = "example.read_status""#, r#"name = "Bad Tool!""#);
        let err = validate_manifest(&raw, "p").unwrap_err();
        assert!(err.contains("tool name") && err.contains("malformed"), "got: {err}");
    }

    // -- (a) validator: rejects over-privileged ------------------------------

    /// A tool requesting a scope OUTSIDE the allowed set is rejected.
    #[test]
    fn validate_rejects_an_unknown_scope() {
        let raw = good_for("p").replace(r#"scopes = ["generate"]"#, r#"scopes = ["root"]"#);
        let err = validate_manifest(&raw, "p").unwrap_err();
        assert!(
            err.contains("unknown capability scope") && err.contains("root"),
            "got: {err}"
        );
    }

    /// A tool requesting `net` while `net_hosts` is EMPTY is OVER-PRIVILEGED — it
    /// claims a network the sandbox profile would deny. Rejected with a precise
    /// over-privilege error. THIS is "a plugin can NOT request a capability
    /// outside the allowed set".
    #[test]
    fn validate_rejects_an_overprivileged_tool() {
        // net scope, but net_hosts is [] in GOOD.
        let raw = good_for("p").replace(r#"scopes = ["generate"]"#, r#"scopes = ["net"]"#);
        let err = validate_manifest(&raw, "p").unwrap_err();
        assert!(
            err.contains("over-privileged") && err.contains("net"),
            "got: {err}"
        );

        // Same for fs_write claimed without an fs_write permission. Build a
        // manifest that DECLARES fs_write scope but has empty fs_write perms.
        let raw = r#"
            [app]
            name = "q"
            version = "0.1.0"
            description = "over-privileged fs_write declaration"
            entry = "q"
            runtime = "binary"

            [permissions]
            fs_write = []

            [[tools.exposes]]
            name = "q.write"
            scopes = ["fs_write"]
        "#;
        let err = validate_manifest(raw, "q").unwrap_err();
        assert!(err.contains("over-privileged") && err.contains("fs_write"), "got: {err}");
    }

    /// A scope that IS backed by the permission set validates (net + a non-empty
    /// net_hosts).
    #[test]
    fn validate_accepts_a_backed_scope() {
        let raw = r#"
            [app]
            name = "netty"
            version = "0.1.0"
            description = "a plugin with a backed net scope"
            entry = "netty"
            runtime = "binary"

            [permissions]
            net_hosts = ["example.com"]

            [[tools.exposes]]
            name = "netty.fetch"
            scopes = ["net"]
        "#;
        assert!(validate_manifest(raw, "netty").is_ok(), "a backed net scope validates");
    }

    // -- (b) handshake: token verification -----------------------------------

    fn key() -> [u8; 32] {
        [7u8; 32] // fixed test key — the handshake is pure over the key.
    }

    /// A valid manifest + a token minted over (name, permissions, nonce) is
    /// ADMITTED with its declared intents.
    #[test]
    fn handshake_admits_a_valid_manifest_with_a_valid_token() {
        let nonce = "launch-nonce-1";
        let manifest = validate_manifest(GOOD, "example-plugin").unwrap();
        let token = compute_token(&key(), manifest.name(), &manifest.permissions, nonce);
        let outcome = register_plugin(GOOD, "example-plugin", &token, &key(), nonce);
        assert_eq!(
            outcome,
            HandshakeOutcome::Admitted {
                name: "example-plugin".to_string(),
                intents: vec!["example.status".to_string()],
            }
        );
    }

    /// A FORGED token (right shape, wrong MAC) fails the handshake — Unauthorized,
    /// not admitted.
    #[test]
    fn handshake_rejects_a_forged_token() {
        let nonce = "launch-nonce-1";
        let forged = "a".repeat(64);
        let outcome = register_plugin(GOOD, "example-plugin", &forged, &key(), nonce);
        assert_eq!(outcome, HandshakeOutcome::Unauthorized);
    }

    /// A token minted for a DIFFERENT permission set (a plugin that widened its
    /// permissions after the token was minted) fails — the token binds the exact
    /// permission set.
    #[test]
    fn handshake_rejects_a_token_bound_to_different_permissions() {
        let nonce = "n";
        // Mint a token over the GOOD permissions...
        let manifest = validate_manifest(GOOD, "example-plugin").unwrap();
        let token = compute_token(&key(), manifest.name(), &manifest.permissions, nonce);
        // ...then present a manifest that WIDENED net_hosts (a different perm set).
        let widened = GOOD.replace("net_hosts = []", r#"net_hosts = ["evil.com"]"#);
        let outcome = register_plugin(&widened, "example-plugin", &token, &key(), nonce);
        assert_eq!(
            outcome,
            HandshakeOutcome::Unauthorized,
            "a widened permission set must break the token binding"
        );
    }

    /// A STALE token (minted under a previous nonce) fails after the nonce rotates.
    #[test]
    fn handshake_rejects_a_stale_nonce_token() {
        let manifest = validate_manifest(GOOD, "example-plugin").unwrap();
        let token = compute_token(&key(), manifest.name(), &manifest.permissions, "old-nonce");
        let outcome = register_plugin(GOOD, "example-plugin", &token, &key(), "new-nonce");
        assert_eq!(outcome, HandshakeOutcome::Unauthorized);
    }

    /// An INVALID manifest fails the handshake before any token check — the
    /// manifest error is surfaced (not a generic Unauthorized).
    #[test]
    fn handshake_rejects_an_invalid_manifest() {
        let bad = good_for("p").replace(r#"scopes = ["generate"]"#, r#"scopes = ["root"]"#);
        let outcome = register_plugin(&bad, "p", "irrelevant", &key(), "n");
        assert!(
            matches!(outcome, HandshakeOutcome::InvalidManifest(e) if e.contains("unknown capability scope")),
            "an invalid manifest must fail with its precise reason"
        );
    }

    // -- (c) the example plugin validates ------------------------------------

    /// The shipped example plugin's manifest (apps/example-plugin/manifest.toml)
    /// validates against the contract — the SDK's own reference is correct.
    #[test]
    fn the_example_plugin_manifest_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("apps/example-plugin/manifest.toml");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let m = validate_manifest(&raw, "example-plugin")
            .expect("the shipped example plugin manifest must validate");
        assert_eq!(m.name(), "example-plugin");
        assert!(!m.intents.provides.is_empty(), "the example declares at least one intent");
        assert!(!m.tools.exposes.is_empty(), "the example exposes at least one tool");
        // Every example tool scope is allowed AND backed (validate_manifest
        // already asserted this; re-state the contract for the reader).
        for tool in &m.tools.exposes {
            for scope in &tool.scopes {
                assert!(ALLOWED_SCOPES.contains(&scope.as_str()), "scope {scope} is allowed");
            }
        }
    }

    #[test]
    fn shipped_microapp_fleet_manifests_validate() {
        // Every Python capability-module app in apps/ must pass the SAME validator
        // the daemon runs at discovery, so each is a real, registrable app:
        // name == dir, dotted-lowercase tool/intent ids, scopes allowed + backed,
        // and every exposed tool READ-ONLY (no consequential surface).
        let apps_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("apps");
        for (name, tool) in [
            ("codeglass", "codeglass.metrics"),
            ("textkit", "textkit.stats"),
            ("hashkit", "hashkit.digest"),
            ("datalint", "datalint.inspect"),
            ("colorlab", "colorlab.analyze"),
            ("cronwise", "cronwise.explain"),
            ("numbase", "numbase.convert"),
        ] {
            let path = apps_dir.join(name).join("manifest.toml");
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            let m = validate_manifest(&raw, name)
                .unwrap_or_else(|e| panic!("{name} manifest must validate: {e}"));
            assert_eq!(m.name(), name);
            assert!(m.tools.exposes.iter().any(|t| t.name == tool), "{name} must expose {tool}");
            assert!(
                m.tools.exposes.iter().all(|t| !t.consequential),
                "{name} tools must be read-only (non-consequential)"
            );
        }
    }
}
