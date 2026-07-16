//! Capability Atlas — a unified, READ-ONLY enumeration of every DARWIN
//! capability, each tagged ARMED (usable now) or INERT (shipped ON, but missing
//! a dependency — a Keychain credential, a data source, …). It is the legibility
//! layer of the self-extension engine: the one place that answers "what can
//! DARWIN actually do right now, and what is one key away?".
//!
//! There is no single capability registry in the daemon, so this module UNIONS
//! the four enumeration surfaces — skills (`skills::global`), agents
//! (`AgentRegistry`), micro-apps (`AppRegistry`), and integrations (the Keychain
//! account allowlist) — and applies each one's own armed/inert primitive,
//! mirroring `selfcheck`'s honest Pass/Skip shape (armed ⇄ Pass, inert ⇄ a
//! missing dependency with a reason).
//!
//! HONESTY: the snapshot is secret-free by construction — it carries only
//! capability NAMES and the PRESENCE (never the value) of each credential. The
//! assembly is a PURE function over injected inputs (`assemble`), unit-tested
//! without ever touching the Keychain; a thin async wrapper (`build_snapshot`)
//! gathers the live inputs (including the async credential probes) and is the
//! only part that reads the real world.

use crate::agents::AgentRegistry;
use crate::apps::AppRegistry;
use crate::config::Config;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashSet;

/// What KIND of capability an atlas entry is. Serializes lowercase so it lines up
/// with the HUD panel's group keys (`skill`/`agent`/`app`/`integration`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CapKind {
    Skill,
    Agent,
    App,
    Integration,
}

/// One capability, with its armed/inert verdict and a one-line, secret-free
/// detail (what it is when armed, or WHY it is inert when not).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapEntry {
    pub name: String,
    pub kind: CapKind,
    pub armed: bool,
    pub detail: String,
}

/// A logical integration and the Keychain account(s) that must ALL be present
/// for it to be armed. The account strings are the exact
/// `integrations::ALLOWED_ACCOUNTS` literals — `resolve_secret` itself guards the
/// allowlist, and the `integration_accounts_are_allowlisted` test below fails the
/// build if any name here ever drifts out of the allowlist (which would make an
/// integration silently, permanently INERT).
struct IntegrationDesc {
    name: &'static str,
    accounts: &'static [&'static str],
}

const INTEGRATIONS: &[IntegrationDesc] = &[
    IntegrationDesc { name: "Cloud (Anthropic)", accounts: &["anthropic_api_key"] },
    IntegrationDesc { name: "Voice (ElevenLabs)", accounts: &["elevenlabs_api_key"] },
    IntegrationDesc { name: "GitHub", accounts: &["github_pat"] },
    IntegrationDesc { name: "Slack", accounts: &["slack_bot_token"] },
    IntegrationDesc {
        name: "Google Workspace",
        accounts: &[
            "google_oauth_client_id",
            "google_oauth_client_secret",
            "google_oauth_refresh_token",
        ],
    },
    IntegrationDesc {
        name: "X (Twitter)",
        accounts: &["x_oauth_client_id", "x_oauth_client_secret", "x_oauth_refresh_token"],
    },
    IntegrationDesc {
        name: "LinkedIn",
        accounts: &[
            "linkedin_oauth_client_id",
            "linkedin_oauth_client_secret",
            "linkedin_oauth_refresh_token",
        ],
    },
    IntegrationDesc {
        name: "Google Ads",
        accounts: &[
            "google_ads_client_id",
            "google_ads_client_secret",
            "google_ads_refresh_token",
            "google_ads_developer_token",
            "google_ads_customer_id",
        ],
    },
    IntegrationDesc {
        name: "Meta Ads",
        accounts: &["meta_app_id", "meta_app_secret", "meta_long_lived_token", "meta_ad_account_id"],
    },
    IntegrationDesc {
        name: "WHOOP",
        accounts: &["whoop_oauth_client_id", "whoop_oauth_client_secret", "whoop_oauth_refresh_token"],
    },
    IntegrationDesc {
        name: "Home Assistant",
        accounts: &["homeassistant_url", "homeassistant_token"],
    },
    IntegrationDesc {
        name: "Plaid (finance read)",
        accounts: &["plaid_client_id", "plaid_secret", "plaid_access_token"],
    },
    IntegrationDesc { name: "Maps", accounts: &["maps_api_key"] },
    IntegrationDesc { name: "Have I Been Pwned", accounts: &["hibp_api_key"] },
];

/// Lightweight, type-decoupled input rows for the PURE assembler, so the unit
/// tests don't need to construct the real `SkillDef`/`Agent`/`AppInfo` types.
pub struct SkillRow<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub source_gated: bool,
}
pub struct AgentRow<'a> {
    pub name: &'a str,
    pub role: &'a str,
}
pub struct AppRow<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub running: bool,
}

/// Trim a multi-line description down to one short, panel-friendly line.
fn short(s: &str) -> String {
    let first = s.trim().split('\n').next().unwrap_or("").trim();
    if first.chars().count() > 100 {
        let mut clipped: String = first.chars().take(99).collect();
        clipped.push('…');
        clipped
    } else {
        first.to_string()
    }
}

/// PURE assembly of the capability set from already-extracted inputs. No I/O, no
/// globals — deterministic and unit-tested. `present_accounts` is the set of
/// Keychain accounts found present (computed by [`build_snapshot`]); an
/// integration is armed only when ALL of its required accounts are in it.
pub fn assemble(
    skills: &[SkillRow],
    skills_enabled: bool,
    agents: &[AgentRow],
    apps: &[AppRow],
    present_accounts: &HashSet<String>,
) -> Vec<CapEntry> {
    let mut out = Vec::with_capacity(skills.len() + agents.len() + apps.len() + INTEGRATIONS.len());

    // SKILLS — a pure skill is armed whenever the library master switch is on; a
    // source-gated one stays inert until its data source is configured.
    for s in skills {
        let armed = skills_enabled && !s.source_gated;
        let detail = if !skills_enabled {
            "inert — skill library off ([skills].enabled = false)".to_string()
        } else if s.source_gated {
            "inert — needs a data source".to_string()
        } else {
            short(s.description)
        };
        out.push(CapEntry { name: s.name.to_string(), kind: CapKind::Skill, armed, detail });
    }

    // AGENTS — every constellation agent is always present; the gating lives on
    // the tools it may invoke (surfaced under Integrations below), so the agent
    // itself reads as armed with its role as the detail.
    for a in agents {
        out.push(CapEntry {
            name: a.name.to_string(),
            kind: CapKind::Agent,
            armed: true,
            detail: short(a.role),
        });
    }

    // MICRO-APPS — a discovered (valid-manifest) app is a registered capability
    // that launches on demand. (Per-binary build detection is a future refinement;
    // we only claim what the registry actually knows.)
    for ap in apps {
        let detail = if ap.running {
            format!("running — {}", short(ap.description))
        } else {
            format!("registered — {}", short(ap.description))
        };
        out.push(CapEntry { name: ap.name.to_string(), kind: CapKind::App, armed: true, detail });
    }

    // INTEGRATIONS — armed only when EVERY required Keychain credential is present.
    for desc in INTEGRATIONS {
        let missing: Vec<&str> =
            desc.accounts.iter().copied().filter(|a| !present_accounts.contains(*a)).collect();
        let armed = missing.is_empty();
        let detail = if armed {
            "connected".to_string()
        } else {
            format!("inert — add in Settings: {}", missing.join(", "))
        };
        out.push(CapEntry {
            name: desc.name.to_string(),
            kind: CapKind::Integration,
            armed,
            detail,
        });
    }

    out
}

/// Wrap the assembled entries into the secret-free `capability.atlas` telemetry
/// payload the HUD panel consumes.
pub fn snapshot(entries: &[CapEntry]) -> Value {
    let armed = entries.iter().filter(|e| e.armed).count();
    json!({
        "enabled": true,
        "armed": armed,
        "total": entries.len(),
        "capabilities": entries,
    })
}

/// LIVE wrapper: gather the four enumeration surfaces (one async credential probe
/// per unique required account) and return the assembled [`CapEntry`] rows. Run
/// from a spawned task at startup — the Keychain probes must never block the boot
/// path. Shared by [`build_snapshot`] (the `capability.atlas` telemetry) and the
/// DARWIN Language Server ([`crate::dls`], capability-name hovers) so both read the
/// SAME live, honest capability set.
pub async fn build_entries(cfg: &Config, agents: &AgentRegistry, apps: &AppRegistry) -> Vec<CapEntry> {
    let reg = crate::skills::global();
    let skills_enabled = cfg.skills.enabled;
    let skill_rows: Vec<SkillRow> = reg
        .all()
        .iter()
        .map(|s| SkillRow { name: s.name, description: s.description, source_gated: s.source_gated })
        .collect();

    let agent_rows: Vec<AgentRow> =
        agents.all().iter().map(|a| AgentRow { name: &a.name, role: &a.role }).collect();

    let app_list = apps.list().await;
    let app_rows: Vec<AppRow> = app_list
        .iter()
        .map(|a| AppRow { name: &a.name, description: &a.description, running: a.running })
        .collect();

    // Probe each UNIQUE required account once (sequential — this runs in a spawned
    // task, and `resolve_secret` is allowlist-guarded with its own 5s timeout, so
    // a hostile/slow Keychain can never wedge the daemon). We record only PRESENCE.
    let mut accounts: Vec<&str> =
        INTEGRATIONS.iter().flat_map(|d| d.accounts.iter().copied()).collect();
    accounts.sort_unstable();
    accounts.dedup();
    let mut present: HashSet<String> = HashSet::new();
    for acct in accounts {
        if crate::integrations::resolve_secret(acct).await.is_some() {
            present.insert(acct.to_string());
        }
    }

    assemble(&skill_rows, skills_enabled, &agent_rows, &app_rows, &present)
}

/// LIVE wrapper: gather the four enumeration surfaces and return the snapshot. Run
/// from a spawned task at startup — the Keychain probes must never block the boot
/// path.
pub async fn build_snapshot(cfg: &Config, agents: &AgentRegistry, apps: &AppRegistry) -> Value {
    snapshot(&build_entries(cfg, agents, apps).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(accts: &[&str]) -> HashSet<String> {
        accts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn skills_armed_only_when_enabled_and_not_source_gated() {
        let skills = [
            SkillRow { name: "hash_text", description: "Hash a string.", source_gated: false },
            SkillRow { name: "stock_quote", description: "Live quote.", source_gated: true },
        ];
        let on = assemble(&skills, true, &[], &[], &present(&[]));
        assert!(on[0].armed, "a pure skill is armed when the library is on");
        assert_eq!(on[0].detail, "Hash a string.");
        assert!(!on[1].armed, "a source-gated skill is inert");
        assert!(on[1].detail.contains("needs a data source"));

        let off = assemble(&skills, false, &[], &[], &present(&[]));
        assert!(!off[0].armed, "the master switch off makes every skill inert");
        assert!(off[0].detail.contains("[skills].enabled = false"));
    }

    #[test]
    fn agents_and_apps_are_registered_capabilities() {
        let agents = [AgentRow { name: "darwin", role: "Prime Orchestrator" }];
        let apps = [
            AppRow { name: "vision", description: "On-device sight.", running: true },
            AppRow { name: "nexus", description: "Audio matrix.", running: false },
        ];
        let out = assemble(&[], true, &agents, &apps, &present(&[]));
        assert_eq!(out[0].kind, CapKind::Agent);
        assert!(out[0].armed);
        assert_eq!(out[0].detail, "Prime Orchestrator");
        assert!(out[1].detail.starts_with("running —"), "a running app says so");
        assert!(out[2].detail.starts_with("registered —"), "an idle app is registered, launch-on-demand");
    }

    #[test]
    fn integration_armed_only_when_all_accounts_present() {
        // GitHub needs exactly one account; supply it -> armed.
        let armed = assemble(&[], true, &[], &[], &present(&["github_pat"]));
        let gh = armed.iter().find(|e| e.name == "GitHub").expect("GitHub entry");
        assert!(gh.armed);
        assert_eq!(gh.detail, "connected");

        // Google Workspace needs three; supply two -> still inert, names the missing one.
        let partial = assemble(
            &[],
            true,
            &[],
            &[],
            &present(&["google_oauth_client_id", "google_oauth_client_secret"]),
        );
        let g = partial.iter().find(|e| e.name == "Google Workspace").expect("Google entry");
        assert!(!g.armed);
        assert!(g.detail.contains("google_oauth_refresh_token"), "names the one missing credential");

        // No credentials -> every integration inert.
        let none = assemble(&[], true, &[], &[], &present(&[]));
        assert!(none.iter().filter(|e| e.kind == CapKind::Integration).all(|e| !e.armed));
    }

    #[test]
    fn snapshot_counts_and_shape() {
        let entries = assemble(
            &[SkillRow { name: "hash_text", description: "Hash.", source_gated: false }],
            true,
            &[AgentRow { name: "darwin", role: "Prime" }],
            &[],
            &present(&["github_pat"]),
        );
        let v = snapshot(&entries);
        assert_eq!(v["total"].as_u64().unwrap() as usize, entries.len());
        let armed = entries.iter().filter(|e| e.armed).count();
        assert_eq!(v["armed"].as_u64().unwrap() as usize, armed);
        assert!(v["capabilities"].is_array());
        assert_eq!(v["enabled"], serde_json::Value::Bool(true));
    }

    #[test]
    fn integration_accounts_are_allowlisted() {
        // Every account this module probes MUST be on the integrations allowlist —
        // otherwise resolve_secret returns None forever and the integration is
        // silently, permanently INERT. This catches allowlist drift at build time.
        for desc in INTEGRATIONS {
            for acct in desc.accounts {
                assert!(
                    crate::integrations::account_is_allowlisted(acct),
                    "atlas integration account {acct:?} is not on the Keychain allowlist",
                );
            }
        }
    }
}
