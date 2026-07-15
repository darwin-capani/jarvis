//! NAMED SFX CUE CATALOG (Phase-2, on top of the existing sound-effect seam).
//!
//! This layer adds a small, CODE-LEVEL (not user-config) catalog of named HUD
//! cues — `confirm`, `alert`, `error`, `success`, `notify`, `wake` — each mapped
//! to a curated ElevenLabs sound-generation PROMPT, plus a by-name [`play_cue`]
//! that resolves the name, GATES exactly like
//! [`crate::main::trigger_sound_effect`] (i.e. on
//! [`crate::voice_tier::sfx_enabled`] = `[voice].cloud_sfx` + a key being
//! present), reuses the existing `op=sound_effect` path, and CACHES the produced
//! WAV by cue name so a repeated cue does NOT re-hit the cloud.
//!
//! WHAT THIS IS NOT: it does NOT add a second gate or a second network seam. The
//! gate predicate and the generation op are the ones already shipped; this module
//! only sits *on top* of them. With the switch off or no key the cue is an HONEST
//! silent no-op ([`PlayOutcome::Disabled`]) — never a fabricated/placeholder cue.
//!
//! SECURITY: the resolved `el_key` is taken only to thread into the request body
//! (server → `xi-api-key`); it is never logged here, never cached, never written
//! to disk, and never part of any telemetry. The cache key is the cue NAME, never
//! the key or the prompt.

use std::path::{Path, PathBuf};

use crate::voice_tier;

/// One catalog entry: a stable cue NAME and its curated ElevenLabs
/// sound-generation PROMPT. The prompt is the only thing that ever leaves the
/// device (text only — no on-device audio is uploaded for a cue).
#[derive(Debug, Clone, Copy)]
pub struct Cue {
    /// The stable lookup name (lowercase, no spaces) an agent/HUD plays by.
    pub name: &'static str,
    /// The curated EL sound-generation prompt for this cue.
    pub prompt: &'static str,
}

/// The BUILT-IN cue catalog: a tasteful, on-brand HUD palette. These are
/// code-level defaults on purpose (no user config map) — the palette is part of
/// the product's voice, not a knob. Names are stable; prompts are curated to be
/// short, clean, synthetic HUD tones (never speech, never musical phrases).
///
/// To keep the catalog a single source of truth, [`cue_prompt`] / [`cue_names`]
/// and the unit tests all read THIS array.
pub const CATALOG: &[Cue] = &[
    Cue {
        name: "confirm",
        prompt: "A soft, single confirmation chime: one clean, short synthetic \
                 bell tone, gentle and reassuring, quick decay, no reverb tail.",
    },
    Cue {
        name: "alert",
        prompt: "An urgent two-tone alert: two quick, bright high-low \
                 electronic beeps in succession, attention-grabbing but not \
                 harsh, crisp and synthetic.",
    },
    Cue {
        name: "error",
        prompt: "A short low error buzz: one brief, dull low-frequency \
                 electronic buzz, flat and negative, no melody, quick stop.",
    },
    Cue {
        name: "success",
        prompt: "A bright ascending success tone: a short, clean rising \
                 three-note synthetic arpeggio, positive and satisfying, light \
                 and sparkly, quick decay.",
    },
    Cue {
        name: "notify",
        prompt: "A gentle notification ping: one soft, rounded synthetic \
                 marimba-like ping, calm and unobtrusive, short and clean.",
    },
    Cue {
        name: "wake",
        prompt: "A subtle power-up swell: a short rising synthetic whoosh into \
                 a soft warm pad, like a HUD coming online, smooth and \
                 futuristic, gentle finish.",
    },
];

/// Resolve a cue NAME to its curated prompt. UNKNOWN name → `None` (the caller
/// MUST surface an honest no-cue — there is never a fabricated/placeholder
/// fallback prompt). Case-insensitive on the name so `"Confirm"` == `"confirm"`.
pub fn cue_prompt(name: &str) -> Option<&'static str> {
    let want = name.trim();
    CATALOG
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(want))
        .map(|c| c.prompt)
}

/// The catalog's cue names, catalog order. Used by the HUD/intent surface to
/// enumerate what can be played, and by the tests to pin the palette.
pub fn cue_names() -> Vec<&'static str> {
    CATALOG.iter().map(|c| c.name).collect()
}

/// Whether `name` is a known cue (case-insensitive).
pub fn is_known_cue(name: &str) -> bool {
    cue_prompt(name).is_some()
}

// ===========================================================================
// Injectable generation seam (mirrors the mcp.rs BoxFuture transport pattern)
// ===========================================================================

/// Boxed-future alias for the object-safe generator seam. The crate avoids
/// `async_trait`; this is the same shape it would desugar to, so a generator is
/// usable behind `&dyn SfxGenerator` and the unit tests can inject a mock that
/// counts calls without any network/Keychain.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The injectable sound-effect generator. Production wires
/// [`crate::inference::InferenceClient`] (its `sound_effect` op); tests wire a
/// counting mock. This is the ONLY network seam a cue touches — there is no
/// second/duplicate seam.
///
/// SECURITY: `el_key` is threaded straight through to the request body; an
/// implementation MUST NOT log it.
pub trait SfxGenerator {
    /// Generate a WAV for `prompt` using `el_key`, returning the produced path.
    fn generate<'a>(
        &'a mut self,
        prompt: &'a str,
        el_key: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<PathBuf>>;
}

impl SfxGenerator for crate::inference::InferenceClient {
    fn generate<'a>(
        &'a mut self,
        prompt: &'a str,
        el_key: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<PathBuf>> {
        // Reuse the EXISTING op=sound_effect path; default shaping hints (the
        // server clamps them) keep cues short and on-brand.
        Box::pin(async move { self.sound_effect(prompt, el_key, None, None).await })
    }
}

// ===========================================================================
// Cache (by cue NAME, under a tmp/cache dir)
// ===========================================================================

/// The cue WAV cache lives under `<root>/state/tmp/sfx-cache/`. The cache key is
/// the cue NAME, so a repeated cue resolves to the stored WAV with NO network
/// call. The cached file is the WAV the server produced for that cue, copied in.
///
/// HONESTY: a cache hit is only honored when the file actually exists on disk; if
/// a cached file was deleted out from under us, the lookup misses and we
/// re-generate (never return a path to a missing file).
pub fn cache_dir(root: &Path) -> PathBuf {
    root.join("state").join("tmp").join("sfx-cache")
}

/// The on-disk cache path for a cue name. Names in the catalog are already safe
/// filename atoms (lowercase ascii, no separators); we still normalize to be
/// defensive so a future/odd name can never escape the cache dir.
fn cache_path(root: &Path, name: &str) -> PathBuf {
    let safe: String = name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    cache_dir(root).join(format!("{safe}.wav"))
}

/// A cache HIT iff the cue's cached WAV exists on disk; returns that path.
pub fn cache_lookup(root: &Path, name: &str) -> Option<PathBuf> {
    let p = cache_path(root, name);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// Store a freshly-generated WAV at `produced` into the cache under `name`,
/// returning the cached path. On any IO error we fall back to the produced path
/// (the cue still plays this time; it just won't be cached) — a cache failure
/// never turns a real cue into a no-op.
fn cache_store(root: &Path, name: &str, produced: &Path) -> PathBuf {
    let dest = cache_path(root, name);
    if let Some(parent) = dest.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return produced.to_path_buf();
        }
    }
    match std::fs::copy(produced, &dest) {
        Ok(_) => dest,
        Err(_) => produced.to_path_buf(),
    }
}

// ===========================================================================
// play_cue — catalog + gate + cache + reuse of the sound_effect seam
// ===========================================================================

/// The result of a by-name cue play. A cue is an honest no-op in two cases
/// (`Disabled`, `Unknown`) and only ever yields a path when a real WAV is
/// available (`Played` from generation, `Cached` from the cache).
#[derive(Debug)]
pub enum PlayOutcome {
    /// A freshly-generated cue WAV (and now cached for next time).
    Played(PathBuf),
    /// A cache HIT: the stored WAV path, returned WITHOUT a network call.
    Cached(PathBuf),
    /// The gate is closed (switch off / no key / offline): an HONEST silent
    /// no-op — never a fabricated cue.
    Disabled,
    /// The name is not in the catalog: an HONEST no-cue — never a fabricated cue.
    Unknown,
    /// The gate was open and this was a cache miss, but the generation seam
    /// failed (network/quota): honest failure, with the message to surface.
    Failed(String),
}

/// Play a named cue.
///
/// Order of operations (and why):
///   1. CATALOG resolve — unknown name → [`PlayOutcome::Unknown`] (no gate read,
///      no network; an unknown cue is never fabricated).
///   2. GATE — `sfx_enabled(cfg, key_present)` AND a non-offline tier, EXACTLY
///      like `trigger_sound_effect`. Closed → [`PlayOutcome::Disabled`] (honest
///      silent no-op). `key_present` is passed in (the caller resolves the key
///      from the Keychain only once the cheap switch checks pass) so the same
///      shipped predicate decides reachability — no duplicate gate.
///   3. CACHE — a hit returns the stored WAV with NO `generate` call.
///   4. GENERATE — a miss reuses the existing `sound_effect` seam, then stores
///      the WAV under the cue name.
///
/// SECURITY: `el_key` is only threaded into `gen.generate`; it is never logged or
/// cached. `offline` is the caller's runtime tier check (a `Local` tier keeps
/// everything on-device); passing it here keeps the offline half of the contract
/// next to the switch+key half.
pub async fn play_cue(
    name: &str,
    cfg: &crate::config::Config,
    offline: bool,
    key: Option<&str>,
    root: &Path,
    gen: &mut dyn SfxGenerator,
) -> PlayOutcome {
    // (1) Catalog resolve FIRST — an unknown cue never reaches the gate/network.
    let Some(prompt) = cue_prompt(name) else {
        return PlayOutcome::Unknown;
    };

    // (2) Gate — identical predicate to trigger_sound_effect: offline → off, and
    //     sfx_enabled = cloud_sfx switch AND a key being present.
    let key_present = key.map(|k| !k.trim().is_empty()).unwrap_or(false);
    if offline || !voice_tier::sfx_enabled(cfg, key_present) {
        return PlayOutcome::Disabled;
    }
    // Gate is open ⇒ a key is present by the predicate above.
    let key = key.expect("sfx_enabled true implies a present key");

    // (3) Cache — a hit short-circuits with NO network call.
    if let Some(hit) = cache_lookup(root, name) {
        return PlayOutcome::Cached(hit);
    }

    // (4) Miss ⇒ reuse the existing sound_effect seam, then cache by name.
    match gen.generate(prompt, key).await {
        Ok(produced) => {
            let stored = cache_store(root, name, &produced);
            PlayOutcome::Played(stored)
        }
        Err(_) => PlayOutcome::Failed(
            "I couldn't play that cue just now — the cloud sound didn't go through. \
             Nothing was produced."
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::cell::Cell;

    /// A counting mock generator: records how many times `generate` was called
    /// and returns a fresh dummy WAV file each time so the cache can store it.
    /// No network, no Keychain, no InferenceClient.
    struct MockGen {
        calls: Cell<u32>,
        dir: PathBuf,
        /// When true, `generate` returns Err (honest-failure path).
        fail: bool,
    }

    impl MockGen {
        fn new(dir: PathBuf) -> Self {
            Self {
                calls: Cell::new(0),
                dir,
                fail: false,
            }
        }
    }

    impl SfxGenerator for MockGen {
        fn generate<'a>(
            &'a mut self,
            _prompt: &'a str,
            _el_key: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<PathBuf>> {
            Box::pin(async move {
                let n = self.calls.get() + 1;
                self.calls.set(n);
                if self.fail {
                    anyhow::bail!("mock failure");
                }
                std::fs::create_dir_all(&self.dir).unwrap();
                let p = self.dir.join(format!("produced-{n}.wav"));
                // A non-empty stand-in WAV so copy/exists checks are real.
                std::fs::write(&p, b"RIFFxxxxWAVE").unwrap();
                Ok(p)
            })
        }
    }

    /// A config with the SFX cue tier ON (the shipped default), so the gate turns
    /// purely on whether a key is present.
    fn cfg_sfx_on() -> Config {
        let mut c = Config::default();
        c.voice.cloud_sfx = true;
        c
    }

    /// A unique temp root per test so the cache dirs don't collide.
    fn tmp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "darwin-sfx-cue-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    // -- catalog shape ------------------------------------------------------

    #[test]
    fn catalog_has_the_expected_cue_names_with_nonempty_prompts() {
        let names = cue_names();
        for expected in ["confirm", "alert", "error", "success", "notify", "wake"] {
            assert!(
                names.contains(&expected),
                "catalog must include the '{expected}' cue"
            );
        }
        // Every prompt is a real, non-empty, non-whitespace prompt.
        for c in CATALOG {
            assert!(
                !c.prompt.trim().is_empty(),
                "cue '{}' must have a non-empty prompt",
                c.name
            );
        }
        // Names are unique.
        let mut seen = std::collections::HashSet::new();
        for c in CATALOG {
            assert!(seen.insert(c.name), "duplicate cue name '{}'", c.name);
        }
    }

    #[test]
    fn cue_prompt_resolves_known_case_insensitively_and_rejects_unknown() {
        assert_eq!(cue_prompt("confirm"), cue_prompt("Confirm"));
        assert!(cue_prompt("confirm").is_some());
        assert!(cue_prompt(" success ").is_some(), "name is trimmed");
        // Unknown name → None (honest no-cue; never a fabricated prompt).
        assert!(cue_prompt("kaboom").is_none());
        assert!(!is_known_cue("kaboom"));
    }

    // -- unknown name → honest no-cue, no gate read, no generation ----------

    #[tokio::test]
    async fn unknown_cue_is_an_honest_no_cue_and_never_generates() {
        let root = tmp_root("unknown");
        let mut gen = MockGen::new(root.join("produced"));
        let out = play_cue("kaboom", &cfg_sfx_on(), false, Some("KEY"), &root, &mut gen).await;
        assert!(matches!(out, PlayOutcome::Unknown));
        assert_eq!(gen.calls.get(), 0, "an unknown cue must NEVER call generate");
    }

    // -- gate: blocks when sfx off / no key / offline ----------------------

    #[tokio::test]
    async fn gate_blocks_when_switch_off() {
        let root = tmp_root("switch-off");
        let mut cfg = cfg_sfx_on();
        cfg.voice.cloud_sfx = false; // switch OFF
        let mut gen = MockGen::new(root.join("produced"));
        let out = play_cue("confirm", &cfg, false, Some("KEY"), &root, &mut gen).await;
        assert!(matches!(out, PlayOutcome::Disabled));
        assert_eq!(gen.calls.get(), 0, "gate-off must not generate");
    }

    #[tokio::test]
    async fn gate_blocks_when_no_key() {
        let root = tmp_root("no-key");
        let mut gen = MockGen::new(root.join("produced"));
        // No key (None) and empty/whitespace key both fail the gate.
        let out_none = play_cue("confirm", &cfg_sfx_on(), false, None, &root, &mut gen).await;
        assert!(matches!(out_none, PlayOutcome::Disabled));
        let out_blank = play_cue("confirm", &cfg_sfx_on(), false, Some("  "), &root, &mut gen).await;
        assert!(matches!(out_blank, PlayOutcome::Disabled));
        assert_eq!(gen.calls.get(), 0, "no-key must not generate");
    }

    #[tokio::test]
    async fn gate_blocks_when_offline() {
        let root = tmp_root("offline");
        let mut gen = MockGen::new(root.join("produced"));
        // Switch on + key present, but offline ⇒ honest no-op.
        let out = play_cue("confirm", &cfg_sfx_on(), true, Some("KEY"), &root, &mut gen).await;
        assert!(matches!(out, PlayOutcome::Disabled));
        assert_eq!(gen.calls.get(), 0, "offline must not generate");
    }

    // -- cache: second play of the same cue does NOT re-hit the seam --------

    #[tokio::test]
    async fn cache_returns_stored_path_without_a_second_generate_call() {
        let root = tmp_root("cache");
        let mut gen = MockGen::new(root.join("produced"));

        // First play: cache MISS ⇒ one generate call, result is cached.
        let first = play_cue("success", &cfg_sfx_on(), false, Some("KEY"), &root, &mut gen).await;
        let first_path = match first {
            PlayOutcome::Played(p) => p,
            other => panic!("expected Played, got {other:?}"),
        };
        assert_eq!(gen.calls.get(), 1, "first play generates exactly once");
        assert!(first_path.is_file(), "cached cue WAV must exist on disk");

        // Second play of the SAME cue: cache HIT ⇒ NO additional generate call.
        let second = play_cue("success", &cfg_sfx_on(), false, Some("KEY"), &root, &mut gen).await;
        let second_path = match second {
            PlayOutcome::Cached(p) => p,
            other => panic!("expected Cached, got {other:?}"),
        };
        assert_eq!(
            gen.calls.get(),
            1,
            "a cache hit must NOT call generate a second time"
        );
        assert_eq!(
            first_path, second_path,
            "the cache must return the stored path"
        );

        // A DIFFERENT cue is a distinct cache key ⇒ a fresh generate call.
        let other = play_cue("notify", &cfg_sfx_on(), false, Some("KEY"), &root, &mut gen).await;
        assert!(matches!(other, PlayOutcome::Played(_)));
        assert_eq!(gen.calls.get(), 2, "a different cue generates again");

        let _ = std::fs::remove_dir_all(&root);
    }

    // -- honest failure: gate open, miss, seam errors ----------------------

    #[tokio::test]
    async fn generation_failure_is_honest_and_not_cached() {
        let root = tmp_root("fail");
        let mut gen = MockGen::new(root.join("produced"));
        gen.fail = true;
        let out = play_cue("alert", &cfg_sfx_on(), false, Some("KEY"), &root, &mut gen).await;
        assert!(matches!(out, PlayOutcome::Failed(_)));
        assert_eq!(gen.calls.get(), 1, "the seam was attempted exactly once");
        // A failure must not have written a cache entry.
        assert!(
            cache_lookup(&root, "alert").is_none(),
            "a failed cue must not be cached"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
