//! The LIVE HONEST CAPABILITY MAP (`capability.map` telemetry).
//!
//! JARVIS ships **armed by default, gated per action**: consequential subsystems
//! are ON but INERT until their dependency is supplied (a key, a model, a TCC
//! grant, an allowlisted folder). That honesty model is easy to misstate in prose
//! and invisible at runtime. This module turns it into a first-class, queryable
//! surface: for every notable subsystem it reports, in plain terms, whether it is
//! `ready`, `armed but needs a dependency`, or `off` — and, crucially, whether the
//! daemon actually **live-probed** that dependency or is only stating the
//! requirement.
//!
//! READ-ONLY and SECRET-FREE, exactly like `policy.snapshot` / `audit.snapshot`:
//! it reports config + a few cheap dependency probes and never mutates anything or
//! emits a key/path/host. The pure [`capability_map`] builder is unit-tested; the
//! async [`emit_map`] gathers the live probes and publishes on the audit-snapshot
//! cadence (see `audit_snapshot_task` in main.rs) for the HUD's CapabilityPanel.
//!
//! `verified` is the anti-fabrication field: `true` means the daemon confirmed the
//! dependency's presence/absence here (cloud key, pdfjail helper, sandbox-exec);
//! `false` means the requirement is stated but NOT checked at this point (a
//! Keychain secret this process doesn't read, or a macOS TCC consent that is only
//! knowable at first use). The map never claims certainty it does not have.

use serde::Serialize;

use crate::config::Config;

/// The honest state of one capability. Serializes lowercase snake_case to match
/// the HUD's `parseCapabilityMap` (`ready` | `armed_needs_dependency` | `off`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapStatus {
    /// Armed AND its dependency is satisfied (or it has none) — usable now.
    Ready,
    /// Armed but inert until a dependency is supplied. `dependency` says which.
    ArmedNeedsDependency,
    /// The subsystem's own switch is off — nothing to do until enabled.
    Off,
}

/// One row of the capability map. SECRET-FREE: `dependency` is a human phrase
/// ("Anthropic API key in Keychain"), never a value/path/host.
#[derive(Debug, Clone, Serialize)]
pub struct Capability {
    /// Stable machine key (e.g. "cloud_reasoning") the HUD/router match on.
    pub key: &'static str,
    /// Human label for the panel.
    pub label: &'static str,
    /// Whether the subsystem's own switch/arming is on (from config).
    pub armed: bool,
    pub status: CapStatus,
    /// What is needed when `armed_needs_dependency`; empty otherwise.
    pub dependency: &'static str,
    /// Did the daemon LIVE-CHECK the dependency here (true), or only state the
    /// requirement without probing it (false)? The honesty flag.
    pub verified: bool,
}

/// The dependency probes the daemon can cheaply and truthfully perform at emit
/// time. Anything not here is reported with `verified = false`.
#[derive(Debug, Clone, Copy)]
pub struct CapDeps {
    /// An Anthropic API key resolved (env or Keychain).
    pub cloud_key: bool,
    /// The pdfjail memory-jail helper is present next to the executable.
    pub pdfjail: bool,
    /// `/usr/bin/sandbox-exec` exists (required to spawn any sandboxed child).
    pub sandbox_exec: bool,
}

/// Build one capability row. `armed` from config; `status`/`verified` from the
/// dependency. A helper so every row is derived the same honest way.
fn cap(
    key: &'static str,
    label: &'static str,
    armed: bool,
    dep: Dep,
) -> Capability {
    let (status, dependency, verified) = match dep {
        // No external dependency: armed => ready.
        Dep::None if armed => (CapStatus::Ready, "", true),
        Dep::None => (CapStatus::Off, "", true),
        // A dependency the daemon LIVE-CHECKED.
        Dep::Probed { present, .. } if armed && present => (CapStatus::Ready, "", true),
        Dep::Probed { need, .. } if armed => (CapStatus::ArmedNeedsDependency, need, true),
        Dep::Probed { .. } => (CapStatus::Off, "", true),
        // A dependency stated but NOT probed here (Keychain secret / TCC consent).
        Dep::Unverified { need } if armed => (CapStatus::ArmedNeedsDependency, need, false),
        Dep::Unverified { .. } => (CapStatus::Off, "", true),
    };
    Capability { key, label, armed, status, dependency, verified }
}

/// How a capability's dependency is determined.
enum Dep {
    /// No external dependency — arming alone makes it ready.
    None,
    /// The daemon live-checked it (`present`), stating `need` when absent.
    Probed { present: bool, need: &'static str },
    /// The requirement is real but NOT probed at this point (`verified=false`).
    Unverified { need: &'static str },
}

/// Build the `capability.map` telemetry payload from the resolved config + the
/// live dependency probes. PURE + total: no globals, no I/O — the exact wire shape
/// the HUD reads is unit-testable. SECRET-FREE.
pub fn capability_map(cfg: &Config, deps: &CapDeps) -> serde_json::Value {
    let master = cfg.integrations.allow_consequential;
    let caps = vec![
        // Cloud reasoning: the bounded Anthropic fallback. Ships ON; inert without
        // a key (the on-device model always answers regardless).
        cap(
            "cloud_reasoning",
            "Cloud reasoning (bounded Anthropic fallback)",
            true,
            Dep::Probed { present: deps.cloud_key, need: "Anthropic API key in Keychain" },
        ),
        // The master gate. Armed => ready, but every consequential action still
        // needs a fresh per-action spoken confirm (stated in the label).
        cap(
            "consequential_actions",
            "Consequential actions (armed; per-action spoken confirm required)",
            master,
            Dep::None,
        ),
        // shell_run: gated by its own switch AND the master, inert without the
        // sandbox-exec binary it must spawn under.
        cap(
            "shell_run",
            "Shell commands (deny-default sandbox, per-action confirm)",
            cfg.shell.enabled && master,
            Dep::Probed { present: deps.sandbox_exec, need: "/usr/bin/sandbox-exec + /bin/sh" },
        ),
        // ui_actuate: gated by its switch AND the master; its real gate is macOS
        // Accessibility TCC, grantable only at first use — never statically known.
        cap(
            "ui_actuate",
            "UI actuation (keystrokes/clicks; per-action confirm)",
            cfg.ui_automation.enabled && master,
            Dep::Unverified { need: "Accessibility TCC consent (granted on-device at first use)" },
        ),
        // On-device file search: armed by its switch, inert until a folder is
        // allowlisted (never a whole-disk scan).
        cap(
            "file_search",
            "On-device file search (docsearch)",
            cfg.docsearch.enabled,
            if cfg.docsearch.roots.is_empty() {
                Dep::Probed { present: false, need: "allowlist a folder in [docsearch].roots" }
            } else {
                Dep::None
            },
        ),
        // Self-distillation (F17, propose-only, never auto-promotes): armed by
        // its switch (ships OFF), inert until the on-device training runtime is
        // present. The daemon can't import Python to confirm mlx-lm + Apple
        // Silicon, so the dependency is UNVERIFIED (verified=false) — never a
        // fabricated "ready"; only the on-device run can truthfully know.
        cap(
            "self_distill",
            "Self-distillation LoRA (staged-only, never auto-promoted)",
            cfg.distill.enabled,
            Dep::Unverified { need: "Apple Silicon + mlx-lm (verified on-device)" },
        ),
        // Federated memory sync (F18, ships OFF, E2E-encrypted): armed by
        // [sync].enabled, inert until a device is paired + a shared key exists.
        // The daemon can't confirm the pairing/key handshake here -> UNVERIFIED
        // (verified=false), never a faked ready.
        cap(
            "federated_sync",
            "Federated memory sync (E2E-encrypted; inert until a device is paired)",
            cfg.sync.enabled,
            Dep::Unverified { need: "a paired device + shared key" },
        ),
        // Acoustic scene awareness (F6, ships OFF, privacy opt-in): armed by
        // [scene].enabled, inert without a bundled sound-event classifier model.
        // No model is shipped, and the daemon can't confirm one at build time ->
        // UNVERIFIED (verified=false), never a faked "listening".
        cap(
            "acoustic_scene",
            "Acoustic scene awareness (ambient sound events; never retains audio)",
            cfg.scene.enabled,
            Dep::Unverified { need: "a bundled sound-event classifier model" },
        ),
        // Self-heal (propose-only): armed by switch, inert without a cloud key to
        // draft the patch.
        cap(
            "self_heal",
            "Self-heal patch drafting (propose-only, human-gated apply)",
            cfg.self_heal.enabled,
            Dep::Probed { present: deps.cloud_key, need: "Anthropic API key in Keychain" },
        ),
        // Forge (propose-only app generation): armed by switch, inert without a
        // cloud key.
        cap(
            "forge",
            "App forge (propose-only micro-app generation)",
            cfg.forge.enabled,
            Dep::Probed { present: deps.cloud_key, need: "Anthropic API key in Keychain" },
        ),
        // Optimize loop: fully on-device (trace recording + proposals) — armed =>
        // ready, no external dependency.
        cap(
            "optimize",
            "Self-optimization loop (on-device, propose-only)",
            cfg.optimize.enabled,
            Dep::None,
        ),
        // Proactive anticipation (EDITH): on-device — armed => ready.
        cap(
            "proactive",
            "Proactive anticipation (EDITH)",
            cfg.proactive.enabled,
            Dep::None,
        ),
        // Plugin-SDK register-on-launch handshake: on-device — armed => ready.
        cap(
            "plugin_sdk",
            "Plugin-SDK launch handshake",
            cfg.plugin_sdk.enabled,
            Dep::None,
        ),
        // MCP tools: armed by switch, inert until a server is configured.
        cap(
            "mcp",
            "MCP tool servers",
            cfg.mcp.enabled,
            if cfg.mcp.servers.is_empty() {
                Dep::Probed { present: false, need: "configure a server in [mcp].servers" }
            } else {
                Dep::None
            },
        ),
        // ElevenLabs cloud voice: the OPTIONAL cloud tier. Ships ON but inert
        // without a key; Kokoro on-device TTS is the private default either way.
        // The key lives in a Keychain account this process does not read here.
        cap(
            "elevenlabs_voice",
            "ElevenLabs cloud voice (optional; Kokoro on-device is the default)",
            true,
            Dep::Unverified { need: "elevenlabs_api_key in Keychain" },
        ),
        // Voice-id owner gate: a fail-closed gate that ships OFF; enrolling is an
        // explicit, on-device act (profile never leaves the device).
        cap(
            "voice_id",
            "Voice-id owner gate (fail-closed; ships OFF)",
            cfg.voice_id.enabled,
            Dep::Unverified { need: "enroll your voice (on-device)" },
        ),
    ];
    serde_json::json!({ "capabilities": caps })
}

/// Gather the live dependency probes and emit `capability.map` for the HUD's
/// CapabilityPanel. READ-ONLY / SECRET-FREE. `resolve_api_key` is cached after
/// startup, so this is cheap on the snapshot cadence.
pub async fn emit_map(cfg: &Config) {
    let deps = CapDeps {
        cloud_key: crate::anthropic::resolve_api_key().await.is_some(),
        pdfjail: crate::docsearch::pdfjail_available(),
        sandbox_exec: std::path::Path::new(crate::apps::SANDBOX_EXEC).exists(),
    };
    // pdfjail is surfaced in detail by docsearch.status; here it only sharpens the
    // file_search story if we later fold it in. Referenced so the probe is honest.
    let _ = deps.pdfjail;
    crate::telemetry::emit("system", "capability.map", capability_map(cfg, &deps));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps(cloud: bool, sandbox: bool) -> CapDeps {
        CapDeps { cloud_key: cloud, pdfjail: false, sandbox_exec: sandbox }
    }

    fn find<'a>(v: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
        v["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["key"] == key)
            .unwrap_or_else(|| panic!("capability {key} present"))
    }

    #[test]
    fn cloud_dependent_caps_flip_with_the_probed_key() {
        let mut cfg = Config::default();
        cfg.self_heal.enabled = true;
        cfg.forge.enabled = true;

        // No key: cloud_reasoning + self_heal + forge are armed but need the key,
        // and the daemon VERIFIED that (probed).
        let m = capability_map(&cfg, &deps(false, true));
        for k in ["cloud_reasoning", "self_heal", "forge"] {
            let c = find(&m, k);
            assert_eq!(c["status"], "armed_needs_dependency", "{k}");
            assert_eq!(c["verified"], true, "{k} probe is verified");
            assert!(c["dependency"].as_str().unwrap().contains("Anthropic"));
        }

        // Key present: all three become ready.
        let m = capability_map(&cfg, &deps(true, true));
        for k in ["cloud_reasoning", "self_heal", "forge"] {
            assert_eq!(find(&m, k)["status"], "ready", "{k}");
        }
    }

    #[test]
    fn shell_needs_master_switch_and_the_sandbox_binary() {
        let mut cfg = Config::default();
        cfg.shell.enabled = true;
        cfg.integrations.allow_consequential = false;
        // Master off => shell is off regardless of the binary.
        assert_eq!(find(&capability_map(&cfg, &deps(true, true)), "shell_run")["status"], "off");

        cfg.integrations.allow_consequential = true;
        // Master on, binary present => ready.
        assert_eq!(find(&capability_map(&cfg, &deps(true, true)), "shell_run")["status"], "ready");
        // Master on, binary MISSING => armed but needs the dependency (verified).
        let c = find(&capability_map(&cfg, &deps(true, false)), "shell_run").clone();
        assert_eq!(c["status"], "armed_needs_dependency");
        assert_eq!(c["verified"], true);
        assert!(c["dependency"].as_str().unwrap().contains("sandbox-exec"));
    }

    #[test]
    fn file_search_is_inert_until_a_root_is_allowlisted() {
        let mut cfg = Config::default();
        cfg.docsearch.enabled = true;
        cfg.docsearch.roots = vec![];
        let c = find(&capability_map(&cfg, &deps(true, true)), "file_search").clone();
        assert_eq!(c["status"], "armed_needs_dependency");
        assert!(c["dependency"].as_str().unwrap().contains("docsearch"));

        cfg.docsearch.roots = vec!["~/Documents".into()];
        assert_eq!(find(&capability_map(&cfg, &deps(true, true)), "file_search")["status"], "ready");
    }

    #[test]
    fn self_distill_ships_off_and_its_device_dep_is_unverified_when_armed() {
        // Ships OFF (config default) -> off.
        let cfg = Config::default();
        assert_eq!(find(&capability_map(&cfg, &deps(true, true)), "self_distill")["status"], "off");

        // Armed -> ArmedNeedsDependency with verified=false: the daemon cannot
        // confirm mlx-lm + Apple Silicon (only the on-device run can), so it
        // never fabricates readiness.
        let mut cfg2 = Config::default();
        cfg2.distill.enabled = true;
        let d = find(&capability_map(&cfg2, &deps(true, true)), "self_distill").clone();
        assert_eq!(d["status"], "armed_needs_dependency");
        assert_eq!(d["verified"], false);
        assert!(d["dependency"].as_str().unwrap().contains("mlx-lm"));
    }

    #[test]
    fn acoustic_scene_ships_off_and_its_model_dep_is_unverified_when_armed() {
        // Ships OFF (config default) -> off.
        let cfg = Config::default();
        assert_eq!(find(&capability_map(&cfg, &deps(true, true)), "acoustic_scene")["status"], "off");

        // Armed -> ArmedNeedsDependency with verified=false: no classifier model
        // is bundled and the daemon can't confirm one at build time.
        let mut cfg2 = Config::default();
        cfg2.scene.enabled = true;
        let d = find(&capability_map(&cfg2, &deps(true, true)), "acoustic_scene").clone();
        assert_eq!(d["status"], "armed_needs_dependency");
        assert_eq!(d["verified"], false);
        assert!(d["dependency"].as_str().unwrap().contains("classifier model"));
    }

    #[test]
    fn unprobed_dependencies_report_verified_false_never_fabricating_presence() {
        let cfg = Config::default();
        let m = capability_map(&cfg, &deps(true, true));
        // ElevenLabs ships armed; its Keychain key is NOT read here -> verified:false.
        let el = find(&m, "elevenlabs_voice");
        assert_eq!(el["status"], "armed_needs_dependency");
        assert_eq!(el["verified"], false, "an unread Keychain dep must not claim certainty");
        // ui_actuate's real gate is TCC, only knowable at first use -> verified:false
        // (armed only when both its switch and the master are on).
        let mut cfg2 = Config::default();
        cfg2.ui_automation.enabled = true;
        cfg2.integrations.allow_consequential = true;
        let ui = find(&capability_map(&cfg2, &deps(true, true)), "ui_actuate").clone();
        assert_eq!(ui["status"], "armed_needs_dependency");
        assert_eq!(ui["verified"], false);
        assert!(ui["dependency"].as_str().unwrap().contains("TCC"));
    }

    #[test]
    fn payload_is_a_flat_capabilities_array_of_the_expected_shape() {
        let m = capability_map(&Config::default(), &deps(false, true));
        let arr = m["capabilities"].as_array().expect("capabilities is an array");
        assert!(arr.len() >= 12, "covers the notable subsystems");
        for c in arr {
            // Every row carries the full honest shape.
            for f in ["key", "label", "armed", "status", "dependency", "verified"] {
                assert!(c.get(f).is_some(), "row missing {f}: {c}");
            }
        }
    }
}
