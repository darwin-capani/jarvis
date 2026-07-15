use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use tracing::warn;

/// Mirrors config/darwin.toml. Every section and key falls back to the
/// contract defaults so the daemon runs even with no config file on disk.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub audio: AudioConfig,
    pub models: ModelsConfig,
    pub router: RouterConfig,
    pub local_tools: LocalToolsConfig,
    pub cloud: CloudConfig,
    pub speech: SpeechConfig,
    pub inference: InferenceConfig,
    pub self_heal: SelfHealConfig,
    pub forge: ForgeConfig,
    pub telemetry: TelemetryConfig,
    pub proactive: ProactiveConfig,
    /// [focus] — FOCUS PROFILES (#24, focus.rs). `profile` ships "default" (the
    /// IDENTITY — today's behavior). A profile is PERMISSION-NEUTRAL: it can only
    /// quiet/narrow which non-consequential proactive intel surfaces, never
    /// loosen a gate, enable a consequential action, or raise autonomy.
    pub focus: FocusConfig,
    pub apps: AppsConfig,
    /// [introspect] — MICRO-APP INTROSPECTION (introspect.rs). `enabled` SHIPS ON
    /// (full-power default). READ-ONLY DEFENSE: a slow sentinel over darwind's OWN
    /// sandboxed children that flags SBPL profile-drift (on-disk tamper) and RSS/
    /// CPU anomalies via sysinfo (same-UID, no entitlement, no ES/ptrace). It
    /// emits telemetry for the HUD/posture and takes NO action — reacting to a
    /// finding would be consequential and rides the existing gates. Inert until an
    /// app runs; with it false the sentinel loop is not spawned (the cheap
    /// record_profile/record_child hooks in apps.rs still populate their maps).
    pub introspect: IntrospectConfig,
    /// [persistence] — PERSISTENCE SENTINEL ("Autoruns for the Mac", persistence.rs).
    /// `enabled` SHIPS ON (full-power default). READ-ONLY DEFENSE: a slow sentinel
    /// that inventories the host's autostart/persistence surfaces (LaunchAgents,
    /// LaunchDaemons, login items, cron, third-party kexts) + each backing binary's
    /// signing/notarization + the Gatekeeper switch via FIXED-ARG bounded
    /// subprocesses, keeps a baseline, and flags what is NEW / REMOVED / newly
    /// UNSIGNED. DARWIN's own two launch items are labeled self, never alarmed on. It
    /// emits `security.persistence` for the HUD/posture and takes NO action —
    /// remediating a finding would be consequential and is out of scope. Honest SKIP
    /// when a read needs a privilege the no-sudo daemon lacks (login items ->
    /// Automation TCC). With it false the sentinel loop is not spawned.
    pub persistence: PersistenceConfig,
    /// [exposure] — INBOUND EXPOSURE AUDITOR (exposure.rs), a defensive
    /// "nmap-of-self". `enabled` SHIPS ON (full-power default). READ-ONLY DEFENSE:
    /// a slow auditor that reads THIS machine's OWN listening socket table via a
    /// FIXED-ARG bounded `netstat -anv` (it sends no packets and never touches
    /// another host), classifies each socket loopback-only vs network-EXPOSED, maps
    /// exposed well-known ports to their macOS sharing service, emits
    /// `security.exposure`, and folds a summary into the posture readout. It takes
    /// NO action — the guided-remediation `open_settings_pane` actuator that opens
    /// the relevant Settings pane stays behind the standard per-action confirm gate.
    /// With it false the auditor loop is not spawned.
    pub exposure: ExposureConfig,
    /// [interception] — TRAFFIC-INTERCEPTION INTEGRITY CHECK (interception.rs), a
    /// defensive "is anything MITMing me?" check. `enabled` SHIPS ON (full-power
    /// default). READ-ONLY DEFENSE: a slow check that reads THIS machine's OWN local
    /// config via FIXED-ARG bounded subprocesses (it sends no packets and never
    /// touches another host) — a system/PAC proxy (`scutil --proxy`), non-default
    /// `/etc/hosts` entries, non-Apple trusted ROOT CAs (`security
    /// dump-trust-settings -d` + the System keychain), the DNS resolvers (`scutil
    /// --dns`), and installed configuration/MDM profiles (`profiles show`). It
    /// explains each finding in plain speech (a rogue trusted root — which silently
    /// breaks ALL TLS — is surfaced loudly), emits `security.interception`, and
    /// folds a summary into the posture readout. It takes NO action — removing a
    /// proxy or a root CA is the user's own action in System Settings / Keychain
    /// Access. Honest SKIP when a read needs a privilege the no-sudo daemon lacks.
    /// With it false the check loop is not spawned.
    pub interception: InterceptionConfig,
    pub integrations: IntegrationsConfig,
    pub standing: StandingConfig,
    /// [drafts] — AUTO-DRAFT (#25, drafts.rs). `enabled` SHIPS ON (full-power
    /// default): proactive drafting is on. A draft is ALWAYS a reviewable PENDING
    /// suggestion — never auto-sent (the module has NO send path) — so enabling
    /// only governs whether DARWIN composes drafts proactively, never whether one
    /// is dispatched.
    pub drafts: DraftsConfig,
    /// [missions] — DURABLE MISSIONS (#26, durable_missions.rs). `durable` SHIPS ON
    /// (full-power default): Fury mission state is persisted. A persisted mission
    /// ALWAYS loads PAUSED (never auto-runs on restart) and every consequential step
    /// re-runs through the gate when resumed — enabling only adds persistence, never
    /// autonomy.
    pub missions: MissionsConfig,
    /// [macros] — MACRO RECORD/REPLAY (#27, macros.rs). `enabled` SHIPS ON
    /// (full-power default): macro record/replay is on. A macro stores ONLY
    /// utterances/intent names (never secrets) and replay re-runs each command
    /// through the normal router + the gate FRESH — enabling only allows
    /// recording/replay, never bypasses the gate.
    pub macros: MacrosConfig,
    pub mcp: McpConfig,
    pub skills: SkillsConfig,
    pub optimize: OptimizeConfig,
    /// [explain] — CAUSA, the causal decision-trace explainer (explain.rs).
    /// `enabled` SHIPS ON (full-power default): the turn loop records a small,
    /// bounded, REDACTED ring of recent decision traces and "why did you do that" /
    /// "why <Agent>" narrates one in persona + emits the secret-free `causa.trace`
    /// telemetry. READ-ONLY — it explains past turns, never changes routing, and
    /// never fabricates a rationale (an unrecorded turn returns an honest empty).
    pub explain: ExplainConfig,
    /// [calibrate] — PLUMBLINE, the confidence-calibration self-report (calibrate.rs).
    /// `enabled` SHIPS ON (full-power default): it is READ-ONLY aggregate analytics
    /// (a reliability curve + ECE gap over the recent confidence/outcome window),
    /// emitting the secret-free `calibrate.report` telemetry — the same always-on,
    /// no-autonomy posture as [episodic] / the eval scorecard. `influence_routing`
    /// SHIPS OFF: it gates the REDUCE-ONLY clarify-band hook, which can only ever
    /// make DARWIN ask MORE clarifying questions in a measurably-overconfident bucket,
    /// never act more boldly — off by default so the first landing is pure analytics.
    pub calibrate: CalibrateConfig,
    /// [mirror] — MIRROR, belief-audit + contest over the SELF-MODEL (user_model.rs).
    /// `enabled` SHIPS ON (full-power default): it is a READ-ONLY / REDUCE-ONLY
    /// surface — "why do you think I prefer X" surfaces the STORED observation,
    /// provenance, and observed-count (never a fabricated reason); "that's wrong
    /// about X" DROPS the belief and writes a suppression tombstone so the
    /// consolidation pass never re-derives it. Contesting only ever REMOVES a shared
    /// `user.model.*` belief and suppresses it — it is structurally unable to touch a
    /// private `agent.*` note, and does nothing consequential. Off => the voice arm
    /// falls through to the model.
    pub mirror: MirrorConfig,
    pub voice_id: VoiceIdConfig,
    /// [threshold] — VOICE-SCOPED GUEST MODE (threshold.rs). `enabled` SHIPS ON
    /// (armed by default): an UNRECOGNIZED speaker (per voice-id) is auto-scoped to
    /// a restrict-only GUEST scope — a strictly READ-ONLY tool allowlist, recall
    /// confined to the SHARED tier (never the owner's private `agent.*` facts), and
    /// a quieter focus profile. Guest scope can ONLY narrow the owner scope; it
    /// LAYERS ON TOP of — and never replaces — the master switch + per-action
    /// confirm + voice-id + policy gates, which are unchanged whether or not guest
    /// mode is on. ARMED-but-INERT: the "unrecognized" signal only exists when
    /// voice-id is ENFORCING (enrolled), so with voice-id off (the shipped default)
    /// this scopes nothing until the owner enrolls a voice or explicitly toggles
    /// guest mode. HONESTY: voice-id is a bar-raiser, not a high-assurance biometric
    /// (replay-spoofable), so guest mode is a COURTESY boundary, not a security
    /// backstop.
    pub threshold: ThresholdConfig,
    pub episodic: EpisodicConfig,
    pub notebooks: NotebookConfig,
    pub lifelog: LifeLogConfig,
    pub voice: VoiceConfig,
    /// [wake] — the CUSTOM WAKE-WORD (#32). `phrase` defaults to "darwin" and
    /// `enabled` SHIPS ON (full-power default) — since the phrase is "darwin",
    /// enabling preserves today's wake behavior exactly. PURE matcher in wake.rs;
    /// the always-listening loop that consults it is DEVICE-gated (mic/TCC).
    pub wake: WakeConfig,
    /// [interpret] — CONTINUOUS LIVE INTERPRETATION (#30). `live` SHIPS ON
    /// (full-power default) — INERT WITHOUT TCC/MIC: the device-gated mic loop feeds
    /// each VAD segment through the PURE interpret_segment pipeline only after
    /// Microphone consent. interpret.speak stays its own opt-in (render-only). The
    /// pure core is in interpret.rs.
    pub interpret: InterpretConfig,
    pub docsearch: DocSearchConfig,
    pub code: CodeConfig,
    /// [shell] — SANDBOXED SHELL / TERMINAL (#43, shell.rs): the HIGHEST-RISK
    /// capability (arbitrary command execution). `enabled` SHIPS ON (full-power
    /// default). Even ON it NEVER auto-runs: a command must clear a conservative
    /// destructive DENYLIST, then PARK as a CONSEQUENTIAL tool for a spoken human
    /// "yes" (shell_run is in NEVER_AUTO_APPROVE_TOOLS — it parks per-action even
    /// under an Always policy), and only ever EXEC under the master switch + confirm
    /// + voice-id + !lockdown — under a DENY-DEFAULT sandbox-exec profile (no network,
    ///   write-confined to a scratch dir, the Keychain / ~/.claude / daemon state
    ///   denied). The exec itself is DEVICE-gated (needs /usr/bin/sandbox-exec + /bin/sh).
    pub shell: ShellConfig,
    /// [ui_automation] — GATED UI AUTOMATION (#44, the CAPSTONE, ui_automation.rs):
    /// the SINGLE MOST DANGEROUS capability (physically actuating the macOS UI —
    /// click/type/key). `enabled` SHIPS ON (full-power default). Even ON it NEVER
    /// auto-runs: EVERY actuation is CONSEQUENTIAL, so it PARKS PER ACTION for a
    /// spoken human "yes" (ONE confirm = ONE actuation; a second re-parks —
    /// ui_actuate is in NEVER_AUTO_APPROVE_TOOLS, so it re-parks even under Always),
    /// and only ever fires under the master switch + confirm + voice-id + !lockdown
    /// — never batched, never autonomous. INERT WITHOUT TCC: the actuation needs
    /// Accessibility TCC consent (runtime, not SBPL-grantable) + a real display.
    pub ui_automation: UiAutomationConfig,
    pub vision: VisionConfig,
    pub image: ImageConfig,
    /// [screen_context] — CONTINUOUS SCREEN CONTEXT (#42, screen_context.rs), the
    /// MOST privacy-sensitive read. `enabled` SHIPS ON (full-power default) — INERT
    /// WITHOUT TCC: the continuous capture loop STILL requires runtime macOS
    /// Screen-Recording consent; the flag cannot grant it, so without consent it
    /// captures nothing. The ring is bounded/redacted/transient (in-RAM only, off
    /// lifelong memory / optimizer / disk) + forgettable, with the WATCHING
    /// indicator; recall is read-only.
    pub screen_context: ScreenContextConfig,
    /// [lumen] — LUMEN: the accessibility SCREEN NARRATOR + hands-free VOICE
    /// NAVIGATION (lumen.rs). `narrate` (continuous focus-change narration) SHIPS
    /// OFF (explicit opt-in; off is a strict no-op); `max_controls` bounds one
    /// readout. Narration is READ-ONLY; a voice action only SELECTS the ONE target
    /// and hands it to the UNCHANGED `ui_actuate` CAPSTONE, which owns every
    /// actuation gate. DEVICE-gated (the locate is the Vision `read.screen`; the
    /// actuation is the capstone's Accessibility-TCC seam).
    pub lumen: LumenConfig,
    pub answers: AnswersConfig,
    pub audit: AuditConfig,
    /// [triage] — FORENSIC TRIAGE SNAPSHOT (triage.rs, aegis). The one-shot
    /// READ-ONLY "capture everything" that freezes a REDACTED, timestamped evidence
    /// bundle under state/forensics/<ts>/ and folds its digest into the audit chain
    /// plus the Keychain external anchor. These knobs only BOUND the capture (bundle
    /// byte budget / log window); RESTORE is never automated, nothing is transmitted.
    pub triage: TriageConfig,
    pub policy: PolicyConfig,
    pub security: SecurityConfig,
    /// [enclave] — ENCLAVE CUSTODY (enclave.rs): ADDITIVE, hardware-bound custody of
    /// the at-rest DB master key. `enabled` SHIPS ON (armed by default) — but INERT
    /// WITHOUT its dependency: minting a non-exportable Secure-Enclave-bound key
    /// needs an Apple Secure Enclave AND the SE entitlement on a code-signed host.
    /// Where present, the master key is wrapped by an SE key OVER the existing
    /// Keychain custody; otherwise custody honestly falls back to the unchanged
    /// OS-protected Keychain path (reported as a self-check SKIP, never a fabricated
    /// "enclave-protected"). It never changes the resolved key or per-agent
    /// credential isolation — custody-hardening only.
    pub enclave: EnclaveConfig,
    /// [distill] — SELF-DISTILLATION (F17, distill.rs): an on-device LoRA
    /// pipeline that learns a personal adapter from the user's OWN graded
    /// interactions. SHIPS OFF (like [security]) because training MUTATES
    /// weights and is device-heavy; it NEVER auto-promotes a trained adapter
    /// into the live model, and the training step is INERT without Apple
    /// Silicon + mlx-lm (reported honestly, never faked).
    pub distill: DistillConfig,
    /// [sync] — E2E-ENCRYPTED FEDERATED SYNC (F18, sync.rs): sync the user's OWN
    /// facts across their OWN devices. SHIPS OFF (like [security]/[distill]);
    /// INERT without a paired peer + a shared key (Keychain only, never config).
    /// The transport is built-but-inert; a bundle never leaves the box unsealed.
    pub sync: SyncConfig,
    /// [scene] — ACOUSTIC SCENE AWARENESS (F6, scene.rs): classify the ambient
    /// soundscape into named sound EVENTS (doorbell, knock, alarm, glass-break…),
    /// distinct from speech capture. SHIPS OFF (continuous ambient listening is a
    /// privacy-consequential act, like [security]/[distill]/[sync]); INERT without
    /// a bundled classifier model (reported honestly, never faked). NEVER retains
    /// raw audio — only event labels + confidences leave the classifier.
    pub scene: SceneConfig,
    /// [overnight] — OVERNIGHT ASYNC AGENTS (F10, overnight.rs): run queued
    /// low-priority tasks while you're AWAY and fold the results into a morning
    /// brief. SHIPS OFF (autonomous unattended work is opt-in). Overnight tasks
    /// are TOOL-LESS — they draft, never act; anything consequential is deferred
    /// to your spoken confirmation on wake. Cloud-gated (needs an API key).
    pub overnight: OvernightConfig,
    /// [webhooks] — WEBHOOK TRIGGERS (#35, webhooks.rs): an INBOUND network
    /// surface. `enabled` SHIPS ON (full-power default) — INERT WITHOUT MAPPINGS +
    /// SECRET: `mappings` ship EMPTY (an unmapped event is rejected, never guessed)
    /// and the HMAC secret resolves from the Keychain (webhook_hmac_secret). EVERY
    /// request is HMAC-authenticated, the `bind` stays 127.0.0.1 loopback (a
    /// non-loopback bind is refused), and a mapped CONSEQUENTIAL intent PARKS for a
    /// spoken confirm — a webhook can never auto-execute a side-effecting action.
    pub webhooks: WebhooksConfig,
    /// [plugin_sdk] — PLUGIN SDK (#36, plugin_sdk.rs): formalizes + VALIDATES the
    /// micro-app capability-module contract (the [intents]/[tools] manifest
    /// block). `enabled` SHIPS ON (full-power default). A plugin still cannot request
    /// a capability outside the allowed set (the validator rejects over-privileged
    /// manifests), cannot escape the default-deny SBPL profile, and any consequential
    /// tool it exposes still rides the gate. The validator itself is PURE (always
    /// available regardless of the flag).
    pub plugin_sdk: PluginSdkConfig,
    /// [power] — BATTERY/THERMAL ADAPTIVE THROTTLING (#38, power.rs). `adaptive`
    /// SHIPS ON (full-power default). PERF-ONLY: the PURE throttle policy only ever
    /// PREFERS the cheaper LOCAL Fast sub-tier / defers heavy work on a low battery
    /// or serious thermal pressure — it never loosens a gate, never makes a cloud
    /// call. The LIVE pmset/thermal reader is device-gated behind this flag.
    pub power: PowerConfig,
    /// [report] — REPORT GENERATION (#40, report.rs). `enabled` SHIPS ON (full-power
    /// default). The op is READ-ONLY — it pulls the already-cited notebook/research
    /// material and folds it into a BOUNDED markdown report, REUSING research.rs's
    /// cite discipline (every citation a REAL source ref an input claim carried; an
    /// uncited claim dropped, never fabricated; no citable source -> an honest-empty
    /// report). It speaks/displays, acts/reaches nothing — safe to enable outright.
    pub report: ReportConfig,
    /// [chart] — DATA -> CHART (#41, chart.rs). `enabled` SHIPS ON (full-power
    /// default). The op is a NEUTRAL presentation act — it serializes a ChartSpec
    /// (the exact data points) as a `chart.data` telemetry envelope the HUD plots
    /// EXACTLY (no interpolation, no invented point, honest axes, honest-empty). It
    /// changes no gate, takes no action, reaches no network — safe to enable
    /// outright.
    pub chart: ChartConfig,
    /// [artifact] — ARTIFACT REGISTRY + PEEK (artifact.rs). `enabled` SHIPS ON
    /// (armed-by-default): producers register the last N things they made into a
    /// BOUNDED, in-memory, on-device recency window with HONEST provenance (the
    /// real producing agent + real citations, or UNCITED), and the read-only `peek`
    /// surface (a voice op + the `artifact_peek` tool) reads them back out as an
    /// `artifact.peek` frame the HUD's QuickLook overlay renders. It opens NO
    /// outward surface, takes NO action, reaches NO network. `registry_size` is the
    /// retention bound (kept last-N).
    pub artifact: ArtifactConfig,
    /// [boundary] — CUSTOMS // EGRESS, a PRE-FLIGHT egress boundary gate
    /// (boundary.rs). `enabled` SHIPS ON (full-power default) as a NEUTRAL
    /// PREVIEW: before a CLOUD turn goes out, CUSTOMS builds a READ-ONLY manifest
    /// of exactly the personal context about to be sent (facts / history / world
    /// rows / persona / system prompt), classifies each by sensitivity, and emits
    /// it as a `boundary.manifest` frame. `default_trim` SHIPS "none" (the
    /// IDENTITY — send everything, today's behavior byte-for-byte). A trim is
    /// REDUCE-ONLY (it can only WITHHOLD whole categories, never add one) and the
    /// LOCAL inference path never reaches CUSTOMS (it egresses nothing), so
    /// enabling only adds an honest inventory + an opt-in trim — never a new
    /// egress.
    pub boundary: BoundaryConfig,
    /// [vault] — VAULT MODE ("go dark", vault.rs). `enabled` SHIPS OFF (vault
    /// removes cloud access, so it is opt-in and never engages silently). With it
    /// active the router forces LOCAL-ONLY routing (no Anthropic-fallback
    /// escalation) and CUSTOMS is forced to its maximal reduce-only trim. It is a
    /// RESTRICT-ONLY tightening — vault can only remove cloud + strengthen the trim,
    /// never add either — toggled at runtime by a `vault` op or a spoken "go dark".
    pub vault: VaultConfig,
    /// [egress] — EGRESS BASELINE + BEACON DETECTOR (egress_beacon.rs), the
    /// longitudinal follow-on to the read-only Egress Sentinel. `enabled` SHIPS ON
    /// (full-power default) — like `[introspect]`/`[audit]` it is pure, READ-ONLY
    /// observability: it samples the SAME lsof outbound snapshot, keeps a BOUNDED
    /// baseline, and runs two PURE classifiers (first-seen talker + regular-interval
    /// beacon cadence). Alerts RIDE EDITH's quiet-hours (`[proactive]`) + cooldown +
    /// debounce so they never spam, and any "block" is PROPOSE-ONLY: a pf rule
    /// rendered as TEXT the user applies with sudo — the loop never mutates the
    /// firewall. UID-scoped (unprivileged lsof sees only same-UID processes; stated
    /// in every frame). With `enabled` false the sampling loop is simply not spawned.
    pub egress: EgressConfig,
    /// [precog] — PRECOG // WHAT-IF, the counterfactual command simulator
    /// (simulate.rs). `enabled` SHIPS ON (full-power default) — it is READ-ONLY by
    /// CONSTRUCTION: a "what would you do if I said X" query runs the SAME pipeline
    /// the live turn would (classify -> selector -> agent -> tier -> gate projection
    /// -> reversibility) UP TO but NEVER THROUGH the confirmation gate, and returns a
    /// PlannedOutcome as a `precog.plan` frame + a spoken summary. The simulate path
    /// holds NO actuator / memory-write / inference handle (SimContext carries only
    /// read views), so it CANNOT fire an action even a benign one — enabling it only
    /// lets DARWIN describe what a real run would do (and that it would PARK), never
    /// act. With it false the "what would you do if ..." cue falls through to normal
    /// routing (it is just another question).
    pub precog: PrecogConfig,
    /// [realm] — SCRATCH REALMS (realm.rs): a disposable, confined build+test
    /// sandbox that VERIFIES a `code_propose_diff` proposal BEFORE a human applies
    /// it. `enabled` SHIPS ON (full-power default) but is INERT WITHOUT DEPS — it
    /// can only run with an allowlisted `[code].roots` repo (the tree it COW-copies)
    /// AND `[shell].enabled` (it reuses the sandboxed-exec seam). The realm is a
    /// network-denied COW copy under `state/realms/<ts>/`; the daemon READS the
    /// user's tree for the copy but NEVER writes it (apply-to-real stays the separate
    /// human-gated `apply_code_diff.sh`). It reports honest UNVERIFIED — never a
    /// faked pass — when the sandbox/tooling is unavailable or no verify command is set.
    pub realm: RealmConfig,
}

/// Every section and key the config knows, for unknown-key diagnostics
/// (audit fix: a typo'd key or section used to be silently ignored with zero
/// signal). MUST stay in lockstep with the section structs below and with
/// config/darwin.toml — including keys consumed only server-side.
/// "mode" under self_heal is part of the self-heal contract
/// ("propose"|"auto") and is listed here so adding it never reads as a typo.
const KNOWN_KEYS: &[(&str, &[&str])] = &[
    ("audio", &["rms_threshold", "silence_ms", "min_speech_ms", "barge_in", "barge_in_rms", "barge_in_ms", "sound_monitor"]),
    // [models] — `vlm` is consumed server-side only (the OPTIONAL on-device VLM
    // for op=describe_image); listed so it never reads as a typo. The
    // multi-resident LOCAL warm-set keys (local_warm/local_budget_gib/local_sizes,
    // task #17) ship CONSERVATIVE: empty + 0 == single-resident.
    ("models", &["llm", "stt", "classifier", "vlm", "local_warm", "local_budget_gib", "local_sizes"]),
    ("router", &["cloud_confidence_threshold", "conversation_route"]),
    // [local_tools] — the OFFLINE bounded tool-loop (Local tier / cloud
    // unreachable). "subset" is an OPTIONAL allow-list override of the curated
    // safe local read/compute tools; an empty/absent list uses the built-in
    // curated subset. "enabled" gates the whole loop; "max_rounds" bounds it.
    ("local_tools", &["enabled", "max_rounds", "subset"]),
    ("cloud", &["fast_model", "heavy_model", "max_tokens"]),
    (
        "speech",
        &[
            "engine",
            "model",
            "voice",
            "speed",
            "openers",
            "sentence_pause_ms",
            "opener_delay_ms",
            "instant_opener",
        ],
    ),
    // [inference] — server-side runtime knobs. `preload` is the existing
    // contract key. SPECULATIVE DECODING (#37): `speculative` SHIPS ON (full-power
    // default) — INERT WITHOUT a loadable `draft_model` (ships ""), in which case
    // generate falls back to normal gen + reports speculative=false. SELECTABLE
    // QUANTIZATION (#39): `quant` ships "auto" (== today's behavior; validated against
    // InferenceConfig::ALLOWED_QUANT, an unknown value falls back to "auto"). Listed
    // so none reads as a typo.
    ("inference", &["preload", "speculative", "draft_model", "quant"]),
    ("self_heal", &["enabled", "mode"]),
    // [forge] — Self-Forge (forge.rs). Same shape and contract as [self_heal]:
    // "mode" is "propose"|"auto" and is listed so it never reads as a typo. Note
    // "auto" NEVER deploys a forged app into apps/ — deploy is ALWAYS a separate
    // human step (scripts/apply_forge.sh); see ForgeConfig.
    ("forge", &["enabled", "mode"]),
    ("telemetry", &["port"]),
    (
        "proactive",
        &[
            "enabled",
            "idle_gap_hours",
            // EDITH anticipation (anticipate.rs). `speak` SHIPS ON (full-power
            // default): EDITH ALSO voices its brief through the echo-safe speech
            // path (never while already speaking), plus the HUD card.
            "speak",
            // Proactive-intelligence suggester (proactive_intel.rs). `suggest`
            // SHIPS ON (full-power default), its OWN gate (not piggybacked on
            // `enabled`): the tick surfaces observed-pattern suggestion cards;
            // accepting one still routes through the gated standing_create confirm.
            "suggest",
            "lead_minutes",
            "unread_floor",
            "quiet_start",
            "quiet_end",
        ],
    ),
    // [focus] — FOCUS PROFILES (#24, focus.rs). `profile` ships "default" (the
    // identity — today's behavior). PERMISSION-NEUTRAL: a profile only quiets
    // which non-consequential proactive intel surfaces, never loosens a gate.
    // `auto` (AUTO-FOCUS) ships OFF: when on, the live tick reselects the profile
    // each tick from on-device signals through the SAME restrict-only path (it can
    // only narrow further, never broaden).
    ("focus", &["profile", "auto"]),
    ("apps", &["autostart"]),
    // [introspect] — the READ-ONLY micro-app introspection sentinel
    // (introspect.rs): SBPL profile-drift + per-app RSS/CPU anomaly surfacing.
    // `enabled` SHIPS ON (full-power default); it only observes darwind's own
    // children (same-UID, no entitlement) and never acts.
    ("introspect", &["enabled", "interval_secs", "startup_delay_secs", "cpu_alert_percent", "rss_growth_ratio"]),
    // [persistence] — the READ-ONLY Persistence Sentinel (persistence.rs): an
    // "Autoruns for the Mac" inventory of autostart surfaces (LaunchAgents /
    // LaunchDaemons / login items / cron / third-party kexts) + per-binary
    // signing/notarization + Gatekeeper, with a baseline diff (new/removed/
    // unsigned). `enabled` SHIPS ON (full-power default); it only reads + reports,
    // never remediates. `assess_signing` gates the codesign/spctl per-binary reads
    // (ASSESSMENT only, never executions); `max_assess` caps how many binaries are
    // assessed per scan.
    ("persistence", &["enabled", "interval_secs", "startup_delay_secs", "assess_signing", "max_assess"]),
    // [exposure] — the INBOUND EXPOSURE AUDITOR (exposure.rs), a READ-ONLY
    // "nmap-of-self" over the local listening socket table (netstat -anv; no
    // packets sent). SHIPS ON. It reports (security.exposure + a posture summary);
    // the only remediation is the gated open_settings_pane actuator.
    ("exposure", &["enabled", "interval_secs", "startup_delay_secs"]),
    // [interception] — the TRAFFIC-INTERCEPTION INTEGRITY CHECK (interception.rs),
    // a READ-ONLY "is anything MITMing me?" read of THIS machine's OWN local config
    // (scutil --proxy / --dns, /etc/hosts, security dump-trust-settings -d + the
    // System keychain, profiles show). No packets sent. SHIPS ON. It reports
    // (security.interception + a posture summary) and closes nothing; honest SKIP
    // when a read needs a privilege the no-sudo daemon lacks.
    ("interception", &["enabled", "interval_secs", "startup_delay_secs"]),
    // [integrations] — `allow_consequential` is THE master gate for outward/
    // side-effecting actions. SHIPS ON (full-power default) — INERT-SAFE: a
    // CONFIRMED consequential action still clears confirm + voice-id + policy +
    // !lockdown at the chokepoints; this only decides whether a confirmed action
    // runs for real vs. returns a DryRun preview.
    ("integrations", &["allow_consequential"]),
    // [standing] — Standing Missions (standing.rs). `enabled` is the subsystem
    // master switch and SHIPS ON (full-power default). Even on, establishing a
    // mission (incl. ARMING a TRIPWIRE) is itself a confirmation-gated action, and
    // every consequential step a run takes still parks behind the confirm gate +
    // allow_consequential, bounded to <=8 active missions under FURY caps — so it can
    // never auto-send/post/spend. The TRIPWIRE (condition-trigger) knobs
    // `condition_eval_secs` (evaluation cadence) + `condition_debounce_secs`
    // (anti-flap re-fire floor) tune the reactive path; listed so neither reads as a
    // typo.
    ("standing", &["enabled", "condition_eval_secs", "condition_debounce_secs"]),
    // [drafts] — AUTO-DRAFT (#25, drafts.rs). `enabled` SHIPS ON (full-power default).
    // A draft is always a reviewable suggestion — the module has no send path, so this
    // flag never enables an autonomous send. `retention` bounds the pending-draft
    // store. Listed so neither key reads as a typo.
    ("drafts", &["enabled", "retention"]),
    // [missions] — DURABLE MISSIONS (#26, durable_missions.rs). `durable` SHIPS ON
    // (full-power default). A persisted mission ALWAYS loads PAUSED (no auto-run on
    // restart) and re-gates each consequential step on resume — this flag governs
    // persistence only, never autonomy. `retention` bounds the mission store. Listed
    // so neither key reads as a typo.
    ("missions", &["durable", "retention"]),
    // [macros] — MACRO RECORD/REPLAY (#27, macros.rs). `enabled` SHIPS ON (full-power
    // default). Replay re-runs each recorded command through the NORMAL router + the
    // gate FRESH (no pre-approval, no batching past the gate); the store holds only
    // utterances + intent names (never a secret). `max_steps` bounds one macro;
    // `retention` bounds the store. Listed so none reads as a typo.
    ("macros", &["enabled", "max_steps", "retention"]),
    // [mcp] — Model Context Protocol client (mcp.rs). `enabled` is the subsystem
    // master switch and SHIPS ON (full-power default) — INERT WITHOUT SERVERS: with an
    // empty `servers` list nothing connects (the installer must NOT add any). The
    // bounds (max_servers / max_tools_per_server / call_timeout_ms /
    // max_output_bytes) cap blast radius. `servers` is an array-of-tables
    // ([[mcp.servers]]); its per-entry keys are validated by McpServerConfig's
    // `deny_unknown_fields` at deserialize time, so only the [mcp] top-level keys
    // are listed here.
    (
        "mcp",
        &[
            "enabled",
            "max_servers",
            "max_tools_per_server",
            "call_timeout_ms",
            "max_output_bytes",
            "servers",
        ],
    ),
    // [skills] — the skill library (skills/). `enabled` is the master switch and
    // SHIPS ON (true): the in-tree skills are PURE + read-only and safe to offer by
    // default. Turning skills off only hides the meta-tools; it does NOT loosen any
    // other gate. A CONSEQUENTIAL skill is still parked behind the cross-turn
    // confirmation gate + the [integrations] allow_consequential master switch (a
    // confirmed action still needs a fresh confirm + voice-id + !lockdown) regardless
    // of this flag — `enabled` controls whether the catalog is OFFERED, never whether
    // a side-effecting skill may fire unconfirmed.
    ("skills", &["enabled"]),
    // [optimize] — the optimization-from-usage loop (optimize.rs). The SAME
    // propose-only contract as [self_heal]/[forge]: `enabled` is the master switch
    // and SHIPS ON (full-power default) — live trace recording is runtime-gated
    // (accrues only while the daemon runs with this on) and PII-redacted. `mode` is
    // "propose"|"auto" and is listed so adding it never reads as a typo; the
    // Trace Store itself never acts on either value, and the downstream Optimizer
    // phase ALWAYS proposes (mode KEEPS "propose") — there is no auto-apply-to-live
    // path, exactly like self-heal's mode.
    ("optimize", &["enabled", "mode"]),
    // [explain] — CAUSA, the causal decision-trace explainer (explain.rs). `enabled`
    // SHIPS ON (read-only observability: a redacted, bounded ring of recent decision
    // traces, narrated by "why did you do that" + emitted as secret-free
    // `causa.trace`). `ring_size` bounds the ring (clamped to a sane band; an
    // out-of-range value never disables recording). Listed so neither reads as a typo.
    ("explain", &["enabled", "ring_size"]),
    // [calibrate] — PLUMBLINE, the confidence-calibration self-report (calibrate.rs).
    // `enabled` SHIPS ON (read-only aggregate analytics: a reliability curve + ECE
    // gap over the recent confidence/outcome window, emitted as secret-free
    // `calibrate.report`). `influence_routing` SHIPS OFF and gates the REDUCE-ONLY
    // clarify-band hook (it can only ever make DARWIN ask MORE, never act bolder).
    // The remaining keys tune the curve resolution + the sample-size honesty floor +
    // the overconfidence dead-band + the widen cap; listed so none reads as a typo.
    (
        "calibrate",
        &["enabled", "influence_routing", "n_bins", "min_sample", "overconfidence_margin", "max_widen"],
    ),
    // [mirror] — MIRROR, belief-audit + contest over the self-model (user_model.rs).
    // `enabled` SHIPS ON (read-only / reduce-only self-model surface: explain a
    // belief with its stored provenance, or contest it — dropping it + writing a
    // suppression tombstone the consolidation pass consults so it is never
    // re-derived). Listed so it never reads as a typo.
    ("mirror", &["enabled"]),
    // [voice_id] — on-device speaker verification (voiceid.rs). `enabled` is the
    // master switch and SHIPS OFF (deliberate: voice-id is a fail-closed GATE, not a
    // feature; enrollment is always explicit). With it false (or with no enrolled
    // profile) NOTHING is gated by voice — behavior is unchanged. `gate_scope` is
    // "consequential"|"all"
    // (unknown -> "consequential"); listed here so it never reads as a typo.
    ("voice_id", &["enabled", "threshold", "min_enroll_samples", "gate_scope"]),
    // [threshold] — voice-scoped GUEST mode (threshold.rs). `enabled` is the master
    // switch and SHIPS ON (armed by default): an unrecognized speaker is auto-scoped
    // to a restrict-only, read-only, shared-recall-only, quieter GUEST scope. It can
    // ONLY narrow the owner scope and LAYERS ON TOP of the unchanged master switch +
    // confirm + voice-id + policy gates. `guest_profile` ("deep_focus" default) is
    // the quiet focus lens a guest turn uses (restrict-only for any value).
    ("threshold", &["enabled", "guest_profile"]),
    // [episodic] — the episodic store (episodic.rs). UNLIKE self_heal/forge/
    // optimize/voice_id, `enabled` SHIPS ON (true): it is the SAME always-on,
    // bounded, local posture as the transcripts table / lifelong-learning fact
    // loop, not an autonomy gate. Recording is still gated per-turn (transient
    // screen-reads + voice-id-unverified + empty turns are never recorded),
    // redacted, agent-scoped, and forgettable. `retention` is the evict-oldest
    // episodes cap (bounded memory); both keys are listed so neither reads as a
    // typo.
    ("episodic", &["enabled", "retention"]),
    // [notebooks] — RESEARCH NOTEBOOKS (notebook.rs): the persistent store of
    // SAGE research runs (a run -> a CITED notebook entry; revisit + append). SAME
    // always-on-but-BOUNDED posture as [episodic] — `enabled` SHIPS ON (true): a
    // notebook is just a persisted, READ-ONLY record of a research run that
    // already happened (cited, redacted, agent-scoped, forgettable), not an
    // autonomy gate. With it false no run is saved and revisit returns an honest
    // empty (never fabricates). `retention` is the evict-oldest ENTRIES cap
    // (bounded memory). Both keys listed so neither reads as a typo.
    ("notebooks", &["enabled", "retention"]),
    // [lifelog] — the LIFE-LOG DIGEST (lifelog.rs): a periodic (daily/weekly)
    // browsable summary built ONLY from the agent-scoped redacted EPISODIC store.
    // SAME always-on-but-bounded posture as [episodic]/[notebooks] — `enabled`
    // SHIPS ON (true): the digest is a READ-ONLY fold over episodes that already
    // exist (never fabricating; empty/sparse window -> honest empty), not an
    // autonomy gate. With it false the digest intent returns an honest "life log
    // is off". It owns no store of its own — forgetting episodes empties it. Listed
    // so the key never reads as a typo.
    ("lifelog", &["enabled"]),
    // [voice] — the OPTIONAL ElevenLabs cloud VOICE TIER (voice_tier.rs). An ADDED
    // TTS layer on top of the on-device Kokoro default, never a replacement.
    // `cloud_tier` SHIPS ON (full-power default) — INERT WITHOUT A KEY: reached only
    // when true AND `elevenlabs_api_key` is in the Keychain AND the model-swap tier
    // is non-Local; otherwise TTS behaves EXACTLY as today (on-device Kokoro, the
    // private default + fallback). When active the TTS text leaves the device.
    // `model` is the ElevenLabs model id (default eleven_flash_v2_5). `voices` is an
    // inline per-agent map (agent name -> EL voice id); an empty/unmapped agent falls
    // back to that agent's Kokoro voice. VOICE-ONLY: DARWIN owns its own
    // brain/router/turn-taking — this tier is TTS, not a hosted Conversational Agents
    // platform. Listed here so neither key reads as a typo; the [voice.voices] table
    // is validated structurally by serde.
    // `cloud_stt` (build 2/2) SHIPS ON (full-power default), the SEPARATE master
    // switch for the ElevenLabs Scribe cloud-STT tier — gated independently of
    // `cloud_tier` (TTS) because STT sends the user's VOICE AUDIO to the cloud (MORE
    // sensitive than TTS text). INERT WITHOUT A KEY: needs the EL key + a non-Local
    // tier; otherwise on-device whisper (the private default + fallback). When active
    // the voice audio leaves the device. Listed here so it never reads as a typo.
    // `adaptive_prosody` (#33) / `whisper` + `whisper_auto` (#34) are the
    // EXPRESSIVENESS flags (prosody.rs), all SHIP ON (full-power default): adaptive
    // prosody shapes EL-v3 audio-tags/stability when the backend is EL-v3-capable
    // (coarse/neutral on Kokoro — EL-v3-gated, never faked); whisper makes replies
    // terse + soft via an explicit command (never silencing a required confirmation);
    // whisper_auto is the separately-gated PURE low-amplitude auto-engage heuristic.
    // Listed so none reads as a typo.
    // `diarize` (#31) SHIPS ON (full-power default), the consumer of EL-Scribe speaker
    // labels — INERT ON-DEVICE: when the active STT backend is EL Scribe (which
    // carries speaker labels), a PURE label-mapper (diarize.rs) renders a
    // multi-speaker transcript; on-device whisper (no diarization model) is an HONEST
    // single-stream "speaker: unknown" labeling — NEVER a fabricated set of distinct
    // speakers. Listed so it never reads as a typo.
    // `cloud_sfx` SHIPS ON (full-power default), gated EXACTLY like cloud_tier
    // (key + non-Local tier): it reaches the inference server's sound_effect cue op;
    // without a key it is a silent no-op (no on-device SFX generator). `stream_tts`
    // is OPT-IN (ships OFF — default behavior unchanged): low-latency streaming TTS
    // on ElevenLabs that falls back to blocking on any streaming error.
    // `pronunciation_dictionary_id` / `pronunciation_dictionary_version` (both
    // DEFAULT "") thread an EL pronunciation-dictionary locator into speak — empty =
    // none (today's speech). All four listed so none reads as a typo.
    // `event_cues` is OPT-IN (ships OFF): when true a fire-and-forget SFX cue plays
    // on key system events (confirm -> "success", deny -> "notify"), riding the same
    // cloud_sfx gate (no key => silent no-op); it never affects the action's outcome
    // or timing. Listed so it never reads as a typo.
    ("voice", &["cloud_tier", "cloud_stt", "model", "voices", "adaptive_prosody", "whisper", "whisper_auto", "diarize", "cloud_sfx", "cloud_music", "stream_tts", "pronunciation_dictionary_id", "pronunciation_dictionary_version", "event_cues", "mic_source"]),
    // [wake] — CUSTOM WAKE-WORD (#32, wake.rs). `enabled` SHIPS ON (full-power
    // default): since `phrase` defaults to "darwin", behavior is identical to today
    // unless the phrase is changed. The always-listening loop that consults the
    // matcher is DEVICE-GATED (mic/TCC). The PURE wake_match
    // (case/punct/whitespace-insensitive + a small edit-distance tolerance; NEVER
    // matches an empty/blank phrase; never triggers on a substring of a larger unrelated
    // word) is in wake.rs; the always-listening loop that calls it is DEVICE-GATED.
    // Listed so neither key reads as a typo.
    ("wake", &["enabled", "phrase"]),
    // [interpret] — CONTINUOUS LIVE INTERPRETATION (#30, interpret.rs). `live` SHIPS ON
    // (full-power default) — INERT WITHOUT TCC/MIC: the DEVICE-GATED mic loop feeds each
    // VAD segment through the PURE interpret_segment (transcribe -> on-device-LLM
    // translate -> render/optionally speak) only after Microphone consent;
    // offline/unavailable degrades HONESTLY (never a fabricated translation), and
    // quality is bounded by the local ~4B model. `source_lang` / `target_lang` are the
    // interpret direction (target defaults to "English"; an empty source = auto-detect).
    // `speak` stays its OWN opt-in (render-only default) for whether the rendered
    // translation is also voiced through the single echo-safe speech path. Listed so
    // none reads as a typo.
    ("interpret", &["live", "speak", "source_lang", "target_lang"]),
    // [docsearch] — on-device file RAG (docsearch.rs): index + search the user's
    // OWN text-like files, 100% on-device. `enabled` SHIPS ON (full-power default) —
    // INERT WITHOUT ROOTS: the folder allowlist (`roots`) ships EMPTY and the
    // installer must NOT guess, so even enabled it indexes NOTHING until the user
    // allowlists a folder (never a whole-disk scan; every candidate file is
    // path-confined under a canonicalized root). The remaining keys are BOUNDS on the
    // index (max files/chunks/bytes, per-file size cap, recursion depth) so the
    // on-disk store stays finite. `build_graph` (knowledge_graph.rs) SHIPS ON
    // (full-power default) — INERT WITHOUT INDEXED DOCS: it runs the deterministic
    // knowledge-graph build only over chunks the confined indexer already produced.
    // It is a real parsed DocSearchConfig field, so it MUST be listed here or the
    // daemon falsely warns "unknown config key docsearch.build_graph ignored" while
    // still honoring it. Listed here so none reads as a typo; the `roots` array is
    // validated structurally by serde.
    (
        "docsearch",
        &[
            "enabled",
            "roots",
            "max_files",
            "max_chunks",
            "max_file_bytes",
            "max_depth",
            "chunk_chars",
            "chunk_overlap",
            "build_graph",
        ],
    ),
    // [code] — CODE INTELLIGENCE (code.rs): code_explain (grounded answers over the
    // docsearch code index, CITED) + code_propose_diff (a PROPOSE-ONLY reviewable
    // unified diff written to state/code/proposals/<ts>/ — it NEVER edits the user's
    // code; the only path that touches code is the human-reviewed
    // scripts/apply_code_diff.sh, confined-by-construction to a [code].roots root).
    // `enabled` SHIPS ON (full-power default) — INERT WITHOUT ROOTS: because it READS
    // and PROPOSES EDITS to the user's code, it does NOTHING until the user allowlists
    // a `roots` codebase root (the installer must NOT guess one). `roots` is the
    // EXPLICIT allowlist of codebase roots (the apply script writes ONLY under a
    // canonicalized root, and code_explain answers only from the docsearch index built
    // over them); EMPTY by default — never an arbitrary path. code_propose_diff
    // drafting also needs the cloud key. `max_diff_bytes` bounds the size of a proposed
    // diff (a bounded artifact). Listed here so none reads as a typo; the `roots` array
    // is validated structurally by serde.
    (
        "code",
        &[
            "enabled",
            "roots",
            "max_diff_bytes",
        ],
    ),
    // [shell] — SANDBOXED SHELL / TERMINAL (#43, shell.rs): the HIGHEST-RISK
    // capability (arbitrary command execution). `enabled` SHIPS ON (full-power
    // default). Even ON it NEVER auto-runs: every command must clear a conservative
    // destructive DENYLIST, then PARK as a consequential tool for a spoken human
    // "yes" (shell_run is in NEVER_AUTO_APPROVE_TOOLS — it parks per-action even under
    // an Always policy), and only ever EXEC under the master switch + confirm +
    // voice-id + !lockdown, inside a DENY-DEFAULT sandbox-exec profile (no network,
    // write-confined to a scratch dir, the Keychain/~/.claude/daemon state denied).
    // INERT WITHOUT DEVICE SUPPORT: the exec needs /usr/bin/sandbox-exec + /bin/sh.
    // Listed here so the key never reads as a typo.
    ("shell", &["enabled"]),
    // [ui_automation] — GATED UI AUTOMATION (#44, the CAPSTONE, ui_automation.rs):
    // the SINGLE MOST DANGEROUS capability (physically actuating the macOS UI —
    // click/type/key). `enabled` SHIPS ON (full-power default). Even ON it NEVER
    // auto-runs: EVERY actuation is CONSEQUENTIAL — it PARKS PER ACTION for a spoken
    // human "yes" (ONE confirm = ONE actuation; a second re-parks; ui_actuate is in
    // NEVER_AUTO_APPROVE_TOOLS, so it re-parks even under Always) and only ever fires
    // under the master switch + confirm + voice-id + !lockdown, never
    // batched/autonomous. INERT WITHOUT TCC: the actuation needs Accessibility consent
    // + a real display. Listed here so the key never reads as a typo.
    ("ui_automation", &["enabled", "actuate_via_app"]),
    // [vision] — the OPTIONAL on-device VISION-LANGUAGE model (VLM) describe path
    // (inference describe_image op + the daemon "describe my screen / what am I
    // looking at / describe this image" intent). `enabled` SHIPS ON (full-power
    // default) — INERT WITHOUT A MODEL: it is DEVICE-GATED (needs mlx-vlm + a
    // multi-GB VLM checkpoint download + enough RAM), and with an empty `model` the
    // op honestly reports "unavailable" until the operator names + downloads one:
    //   - `enabled` (SHIPS ON, full-power default): master switch. INERT WITHOUT A
    //     MODEL — with an empty `model` the describe intent falls back honestly
    //     (OCR/classification or "the model isn't downloaded").
    //   - `model` (SHIPS EMPTY): the VLM repo id ([models].vlm-style). EMPTY =>
    //     the server has no VLM to load and the op returns the honest unavailable
    //     structure; the daemon never fabricates a description.
    // The image is read ON-DEVICE by the inference server (pixels never leave the
    // device); DISTINCT from OCR (read.screen = text glyphs; VLM = visual
    // understanding). Listed here so neither key reads as a typo.
    ("vision", &["enabled", "model"]),
    // [image] — the OPTIONAL on-device TEXT->IMAGE generation path (task #18):
    // the inference `generate_image` op (MLX diffusion) plus the daemon
    // "generate/make/draw an image of X" intent. `enabled` SHIPS ON (full-power
    // default) — INERT WITHOUT A MODEL.
    //   - `enabled` (SHIPS ON, full-power default): master switch. INERT WITHOUT A
    //     MODEL — with an empty `model` the generate-image intent surfaces an honest
    //     "the on-device image model isn't set up" line.
    //   - `model` (SHIPS EMPTY): the on-device diffusion model id (a
    //     FLUX.1-schnell-class mflux checkpoint). EMPTY => the server has no
    //     image model to load and the op returns the honest unavailable structure;
    //     the daemon NEVER fabricates an image and NEVER calls a cloud image API.
    // The prompt + the generated pixels stay ON-DEVICE (image generation is LOCAL
    // only — NO cloud image API). Listed here so neither key reads as a typo.
    ("image", &["enabled", "model"]),
    // [screen_context] — CONTINUOUS SCREEN CONTEXT (#42). `enabled` SHIPS ON
    // (full-power default) — INERT WITHOUT TCC: the continuous capture loop STILL
    // requires runtime macOS Screen-Recording consent; without it the ring never
    // grows and no WATCHING indicator fires. `interval_secs` (cadence, floored >= 1)
    // and `cap` (the hard ring bound, evict-oldest, floored >= 1) tune the loop/ring.
    // The ring is redacted + transient (in-RAM only, off lifelong memory / optimizer /
    // disk) + forgettable; recall is read-only. Listed so no key reads as a typo.
    ("screen_context", &["enabled", "interval_secs", "cap"]),
    // [lumen] — LUMEN: the accessibility SCREEN NARRATOR + hands-free VOICE
    // NAVIGATION (lumen.rs).
    //   - `narrate` (SHIPS OFF): CONTINUOUS focus-change narration is EXPLICIT
    //     opt-in; off is a strict no-op (Lumen speaks nothing on its own). The
    //     explicit "read me the screen" request path is unaffected.
    //   - `max_controls` (DEFAULT 20, floored >= 1): the hard bound on how many
    //     on-screen controls one readout narrates / offers for selection.
    // Narration is READ-ONLY; a voice action only SELECTS the ONE target and hands
    // it to the UNCHANGED `ui_actuate` CAPSTONE (which owns every actuation gate).
    // Listed here so neither key reads as a typo.
    ("lumen", &["narrate", "max_controls"]),
    // [answers] — answer annotations (anthropic.rs `answers` module): the
    // always-cite source-tracking (#5) + the self-reported confidence (#8). ALL SHIP
    // ON (full-power default):
    //   - `cite` (true): surface the REAL tool-result sources that fed a turn as a
    //     "Sources:" line — or "from my own knowledge" when no retrieval ran (never
    //     a fabricated citation).
    //   - `confidence` (true): ask the model to self-report grounded/inferred/
    //     uncertain + a one-line why, parsed + surfaced. The PLUMBING is gated; the
    //     model's calibration is runtime/model-behavior-gated (never claimed
    //     measured).
    //   - `verify` (true): the self-verification pass (#7). On an IMPORTANT turn,
    //     ONE extra self-critique of the draft against the real sources + AT MOST
    //     one bounded revise (skips trivial turns; needs the cloud tier for the cloud
    //     path). A second check REDUCES hallucination; it is NOT a correctness
    //     guarantee, and it costs one extra model call.
    //   - `cross_check` (true): #21 tool-result verification. A BOUNDED plausibility
    //     cross-check of a TOOL RESULT before it is surfaced as fact / built into a
    //     consequential action — deterministic sanity checks (empty-vs-claimed,
    //     uncited fact, self-contradiction, out-of-range) always run; it only
    //     DOWNGRADES confidence + FLAGS, NEVER removes a confirmation gate.
    //   - `cross_check_model_pass` (true): #21 optional single bounded "does this
    //     result look right?" model call, gated UNDER `cross_check` (a latency/cost
    //     add; needs the cloud tier for the cloud path).
    //   - `debate` (true): #22 multi-model debate. For HIGH-STAKES asks only, a
    //     SECOND independent model answers the same question; agreement RAISES
    //     confidence, disagreement SURFACES BOTH (never picked/averaged), an
    //     unavailable second brain falls back to one + says so. <=2 model calls;
    //     needs the cloud tier; inert on ordinary turns.
    // Listed here so none of the keys reads as a typo.
    (
        "answers",
        &["cite", "confidence", "verify", "cross_check", "cross_check_model_pass", "debate"],
    ),
    // [audit] — the append-only, hash-chained, tamper-EVIDENT consequential-action
    // audit log (audit.rs). UNLIKE the autonomy switches (self_heal/forge/...),
    // `enabled` SHIPS ON (true): the log is READ-ONLY ACCOUNTABILITY — it never
    // takes an action, only records the decisions the consequential gate already
    // makes, secret-free and bounded. It is on-but-bounded (defensible: a record-
    // only ledger loosens nothing). With it false NO entry is written and the
    // chokepoints behave byte-for-byte as today (the audit calls are skipped).
    // `max_entries` bounds retention (prune-oldest + re-root past the cap). Listed
    // here so neither key reads as a typo.
    ("audit", &["enabled", "max_entries"]),
    // [triage] — FORENSIC TRIAGE SNAPSHOT (triage.rs, aegis). The one-shot
    // READ-ONLY "capture everything" that freezes a REDACTED evidence bundle under
    // state/forensics/<ts>/ and folds its digest into the audit chain + Keychain
    // external anchor. `max_bundle_bytes` bounds the bundle; `log_window_minutes`
    // bounds the security-subsystem `log show` excerpt. Listed so neither reads as a
    // typo. It only reads + reports; RESTORE is never automated, nothing is transmitted.
    ("triage", &["max_bundle_bytes", "log_window_minutes"]),
    // [policy] — the per-action policy store (policy.rs). The controlled, USER-SET
    // loosening/hardening that sits BENEATH the [integrations] master switch. It
    // SHIPS EMPTY: no rules => `evaluate` returns Ask for every action, so the
    // three consequential chokepoints behave EXACTLY as today (ASK/park
    // everywhere). Rules are USER-SET ONLY (Settings / the authenticated-local
    // command channel) — there is NO tool/agent/model path that can write a
    // policy. `enabled` is the master switch for the layer (ships ON, but inert
    // while the store is empty); with it false the layer is bypassed and every
    // action is Ask regardless of any saved rule. The rules themselves live in the
    // user-owned state/policy.json, NOT in this TOML (so the model can never reach
    // them via a config edit either). Listed here so the key never reads as a typo.
    ("policy", &["enabled"]),
    // [security] — AT-REST ENCRYPTION of the sensitive local stores (#11; crypto.rs
    // + the per-store `open_encrypted` seam). `encrypt_memory` is the master switch
    // and SHIPS OFF (false), a deliberate operator opt-in (enabling rewrites the
    // on-disk format + is irreversible — lose the Keychain item and the DBs are
    // unrecoverable). With it false EVERY store opens via its plaintext
    // `open(path)` with NO `PRAGMA key` — byte-for-byte today's plaintext SQLite.
    // When the operator flips it true, a fresh 256-bit master key is generated +
    // stored in the macOS Keychain (account `memory_encryption_key`), the existing
    // plaintext stores are re-keyed to SQLCipher (migration), and every subsequent
    // open uses `open_encrypted`. HONESTY: SQLCipher protects AT REST ON DISK only
    // — NOT against a live-process/root attacker (key + decrypted pages are in RAM
    // while the daemon runs); the config TOML and the Keychain item itself are not
    // covered; lose the Keychain item => the DBs are unrecoverable. Listed here so
    // the key never reads as a typo.
    ("security", &["encrypt_memory"]),
    // [enclave] — ENCLAVE CUSTODY (enclave.rs). `enabled` SHIPS ON (armed by
    // default) but is INERT WITHOUT the Secure Enclave + SE entitlement: it wraps
    // the at-rest master key with a non-exportable hardware-bound SE key WHERE
    // present, else custody honestly falls back to the unchanged OS-protected
    // Keychain path (a self-check SKIP, never a fabricated enclave claim). Listed
    // here so the key never reads as a typo.
    ("enclave", &["enabled"]),
    ("distill", &["enabled", "python", "base_model", "iters"]),
    ("sync", &["enabled", "peer_endpoint"]),
    ("scene", &["enabled", "confidence_floor"]),
    ("overnight", &["enabled", "min_gap_secs"]),
    // [webhooks] — WEBHOOK TRIGGERS (#35, webhooks.rs). An INBOUND network surface.
    // `enabled` SHIPS ON (full-power default) — INERT WITHOUT MAPPINGS + SECRET: even
    // on, an unmapped event is rejected and the HMAC secret must be present in the
    // Keychain, so nothing is accepted until the user adds a mapping + sets the
    // secret. `bind` is the listen address (defaults to 127.0.0.1 loopback; a
    // non-loopback value is refused at bind time). The HMAC secret is
    // resolved from the Keychain (account `webhook_hmac_secret`), NEVER inlined here.
    // `mappings` is an array-of-tables ([[webhooks.mappings]]) of explicit
    // event->intent allowlist entries; its per-entry keys are validated by
    // WebhookMapping's `deny_unknown_fields` at deserialize time, so only the
    // [webhooks] top-level keys are listed here. `max_body_bytes` bounds a request.
    (
        "webhooks",
        &[
            "enabled",
            "bind",
            "port",
            "max_body_bytes",
            "mappings",
        ],
    ),
    // [plugin_sdk] — PLUGIN SDK (#36, plugin_sdk.rs). The capability-module
    // contract validator + register-on-launch handshake. `enabled` SHIPS ON
    // (full-power default): the live launch handshake scopes a plugin's declared
    // intents/tools (the validator itself is pure and always available regardless).
    // A plugin still can't request a capability outside the allowed set, can't escape
    // the default-deny SBPL profile, and any consequential tool it exposes still
    // rides the gate — so enabling is safe. Listed so the key never reads as a typo.
    ("plugin_sdk", &["enabled"]),
    // [power] — BATTERY/THERMAL ADAPTIVE THROTTLING (#38, power.rs). `adaptive`
    // SHIPS ON (full-power default). PERF-ONLY: the conservative policy may prefer the
    // cheaper local Fast sub-tier + defer heavy work on low battery / serious thermal;
    // it never loosens a gate, never makes a cloud call. The live pmset/thermal read
    // is device-gated behind this flag. `low_battery_pct` is the discharge threshold
    // below which (on battery) DARWIN prefers Fast + defers heavy work. Listed so
    // neither reads as a typo.
    ("power", &["adaptive", "low_battery_pct"]),
    // [report] — REPORT GENERATION (#40, report.rs). `enabled` SHIPS ON (full-power
    // default). The read-only op folds already-cited notebook/research material into a
    // bounded markdown report under research.rs's cite discipline (every citation a
    // REAL source ref; uncited claims dropped; no citable source -> honest-empty). It
    // speaks/displays, reaches nothing — safe to enable outright. Listed so the key
    // never reads as a typo.
    ("report", &["enabled"]),
    // [chart] — DATA -> CHART (#41, chart.rs). `enabled` SHIPS ON (full-power
    // default). It serializes a ChartSpec (the EXACT data points) as a neutral
    // `chart.data` telemetry envelope the HUD plots exactly (no interpolation/invented
    // point, honest axes/empty); it changes no gate, takes no action, reaches no
    // network — safe to enable outright. Listed so the key never reads as a typo.
    ("chart", &["enabled"]),
    // [artifact] — ARTIFACT REGISTRY + PEEK (artifact.rs). `enabled` SHIPS ON
    // (armed-by-default): producers register the last N results into a BOUNDED,
    // in-memory, on-device recency window with HONEST provenance (real agent + real
    // citations, or UNCITED), and the read-only `peek` surface reads them back out.
    // `registry_size` is the retention bound (kept last-N). Opens no outward surface,
    // takes no action, reaches no network. Listed so neither key reads as a typo.
    ("artifact", &["enabled", "registry_size"]),
    // [boundary] — CUSTOMS // EGRESS (boundary.rs). `enabled` SHIPS ON (a neutral
    // READ-ONLY preview of the cloud egress manifest); `default_trim` SHIPS "none"
    // (the identity — send everything, today's behavior). A trim is REDUCE-ONLY
    // ("none" | "no_facts" | "no_memory"); an unknown value degrades to "none".
    // Listed so neither key reads as a typo.
    ("boundary", &["enabled", "default_trim"]),
    // [vault] — VAULT MODE ("go dark", vault.rs). `enabled` SHIPS OFF (vault removes
    // cloud access, so it is opt-in). Listed so the key never reads as a typo.
    ("vault", &["enabled"]),
    // [egress] — EGRESS BASELINE + BEACON DETECTOR (egress_beacon.rs). `enabled`
    // SHIPS ON (read-only observability). The remaining keys tune the bounded
    // baseline retention and the beacon-cadence + alert-suppression thresholds;
    // quiet-hours is inherited from [proactive], not repeated here. Listed so a
    // key never reads as a typo.
    (
        "egress",
        &[
            "enabled",
            "startup_delay_secs",
            "sample_interval_secs",
            "retention_secs",
            "max_talkers",
            "max_samples_per_talker",
            "beacon_min_samples",
            "beacon_min_interval_secs",
            "beacon_max_interval_secs",
            "beacon_max_jitter",
            "alert_cooldown_secs",
            "alert_min_gap_secs",
        ],
    ),
    // [precog] — PRECOG // WHAT-IF, the counterfactual command simulator
    // (simulate.rs). `enabled` SHIPS ON (full-power default) — READ-ONLY by
    // construction: it describes what a real run WOULD do (and that it would PARK),
    // never acts. Listed so the key never reads as a typo.
    ("precog", &["enabled"]),
    // [realm] — SCRATCH REALMS (realm.rs): a disposable, confined build+test sandbox
    // that VERIFIES a code_propose_diff proposal in a network-denied COW copy of the
    // codebase BEFORE a human applies it. `enabled` SHIPS ON (full-power default) —
    // INERT WITHOUT DEPS: it needs an allowlisted [code].roots repo + [shell].enabled.
    // `verify_command` SHIPS EMPTY (the operator names their project's build/test
    // command; empty => an honest UNVERIFIED, never a faked pass). Listed here so
    // neither key reads as a typo.
    ("realm", &["enabled", "verify_command", "timeout_secs"]),
];

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub rms_threshold: f64,
    pub silence_ms: u64,
    pub min_speech_ms: u64,
    /// Barge-in: let the user interrupt DARWIN mid-reply by speaking over him.
    pub barge_in: bool,
    /// RMS the user's voice must exceed DURING playback to count as a barge-in —
    /// set well ABOVE rms_threshold so DARWIN's own voice through the speakers
    /// (echo) cannot trip it. Device/volume dependent; tune on the real Mac
    /// (raise it if DARWIN cuts himself off; lower it if barge-in won't trigger).
    pub barge_in_rms: f64,
    /// How long (ms) the user must stay above barge_in_rms before DARWIN stops —
    /// a dwell so a cough/click/transient never cuts him off.
    pub barge_in_ms: u64,
    /// Ambient sound monitor (task #15). When ON (and macOS mic/TCC
    /// consent is granted on-device) the daemon PERIODICALLY classifies a short
    /// ambient audio clip through the Vision app's on-device `classify.sound` op
    /// (Apple Sound Analysis, the fixed ~300-class SNClassifierIdentifier.version1)
    /// and emits sound-class events (name-called / doorbell / alarm / glass-break).
    /// SHIPS ON (full-power default) — INERT WITHOUT MIC/TCC: it cannot capture
    /// anything until the user grants Microphone consent (macOS TCC); the flag cannot
    /// grant it. Only the sound-class LABELS (+ confidence) are ever emitted, the
    /// AUDIO never leaves the device. DISTINCT from STT (speech); the one-shot "what
    /// was that sound" intent on an already-captured clip works regardless.
    pub sound_monitor: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            rms_threshold: 0.015,
            silence_ms: 350,
            min_speech_ms: 250,
            barge_in: true,
            barge_in_rms: 0.06,
            barge_in_ms: 250,
            // SHIPS ON (full-power default) — INERT WITHOUT MIC/TCC: the ambient
            // sound monitor needs on-device Microphone consent (macOS TCC) before
            // it can capture anything; the flag cannot grant that. Only the
            // sound-class LABELS leave the op, the AUDIO never leaves the device,
            // and it is DISTINCT from STT. The one-shot "what was that sound"
            // intent works regardless of this switch.
            sound_monitor: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    pub llm: String,
    pub stt: String,
    /// Dedicated small resident model for op=classify; "" = reuse the main
    /// LLM. Consumed server-side; mirrored here so the Default impl stays in
    /// lockstep with darwin.toml. Gated: only set after a candidate passes
    /// the 7-utterance accuracy eval (>=6/7, all heavy cases heavy).
    #[allow(dead_code)] // shared contract; read by the inference server
    pub classifier: String,
    /// MULTI-RESIDENT LOCAL warm-set (task #17): OPTIONAL extra local model ids
    /// the inference server keeps WARM alongside the base [models].llm so the
    /// Local tier can swap between them INSTANTLY (no reload) — a "local-fast"
    /// model for trivial offline turns and the capable base for harder ones.
    /// DEFAULT is EMPTY == single-resident: only `llm` is warm, exactly today's
    /// behavior and the safe state on a low-RAM Mac. Multi-resident is OPT-IN
    /// and RAM-BOUNDED (see `local_budget_gib`): the server admits an extra only
    /// while the running footprint estimate stays within budget, else it stays
    /// single-resident. Mirrors [models].local_warm in darwin.toml + server.py.
    pub local_warm: Vec<String>,
    /// RAM budget (GiB of unified memory) the local warm-set may occupy. 0 (the
    /// CONSERVATIVE default) or any non-positive value => SINGLE-RESIDENT: only
    /// the base `llm` is kept warm regardless of `local_warm`. A positive budget
    /// lets the policy admit extras until their estimated footprints would exceed
    /// it. HONEST: two warm models cost ~2x RAM; the default keeps a low-RAM Mac
    /// (8GB M1) unaffected. The ESTIMATE drives only keep-warm bookkeeping — it is
    /// not a measurement and the swap speed benefit is device-gated.
    pub local_budget_gib: f64,
    /// OPTIONAL id -> approx resident GiB overrides for the budgeting policy,
    /// used when the coarse heuristic would mis-estimate a model. Mirrors
    /// [models].local_sizes; consumed both here (the HUD telemetry plan) and by
    /// server.py (the real keep-warm manager).
    pub local_sizes: std::collections::BTreeMap<String, f64>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            llm: "mlx-community/Qwen3-4B-Instruct-2507-4bit".to_string(),
            stt: "mlx-community/whisper-small-mlx".to_string(),
            classifier: String::new(),
            // CONSERVATIVE single-resident default: no extra warm models, a 0
            // budget. A Mac left at the defaults keeps exactly one local model
            // warm (today's behavior). Multi-resident is opt-in + RAM-bounded.
            local_warm: Vec::new(),
            local_budget_gib: 0.0,
            local_sizes: std::collections::BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RouterConfig {
    pub cloud_confidence_threshold: f64,
    /// Where the CONVERSATION intent (casual chat, greetings, opinions — the
    /// llm_voice conversation path, NOT actions/stats/memory ops) is answered.
    /// "cloud_heavy" (the default): cloud Opus ([cloud].heavy_model) for
    /// genuinely varied, human personality — the local 4B is near-deterministic
    /// on bare greetings (a model-capacity ceiling). "cloud_fast": cloud Haiku
    /// ([cloud].fast_model). "local": the resident 4B (offline/Hulk path).
    /// The cloud variants require the cloud key — with no key, or on a cloud
    /// error, conversation degrades to the local 4B. Unknown values behave as
    /// "local" (the safe, always-available path). One line flips the brain.
    pub conversation_route: String,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            cloud_confidence_threshold: 0.6,
            conversation_route: "cloud_heavy".to_string(),
        }
    }
}

/// The OFFLINE bounded tool-loop (task #3). When the conversation tier resolves
/// to Local (the "work offline" override, no cloud key, or a cloud-unreachable
/// fallback), the on-device 4B is OFFERED a CURATED SAFE local-tool subset and
/// run in a BOUNDED loop: prompt -> parse the 4B's tool call -> execute it
/// through the SAME gated `execute_tool` (so the consequential confirmation gate,
/// the voice-id gate, lockdown and per-action policy ALL still apply offline) ->
/// feed the result back -> at most `max_rounds` rounds. There is NO benefit /
/// chit-chat classifier gate: the subset is offered on every Local turn, the 4B
/// uses a tool when its reply parses as one, and otherwise the loop falls back
/// gracefully to a plain converse answer.
///
/// Defaults are conservative: ON (the offline path gains agency over SAFE local
/// tools only), 3 rounds, and the BUILT-IN curated subset (an empty `subset`).
/// The subset is local READ/COMPUTE only — it can never list an outward/cloud
/// tool (gmail/slack/web/etc.); a configured `subset` is INTERSECTED with the
/// curated safe set, so a misconfiguration can only ever NARROW it, never widen
/// it past the safe boundary. The cloud tool loop is entirely separate and
/// unchanged by these knobs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LocalToolsConfig {
    /// Engage the offline tool-loop at all. When false, a Local-tier
    /// conversation turn answers with today's plain converse (no tool use).
    pub enabled: bool,
    /// Hard ceiling on the number of (prompt -> tool) rounds before the loop is
    /// forced to a plain text answer. Bounded — there is no unbounded loop.
    pub max_rounds: u32,
    /// OPTIONAL allow-list override. Empty (the default) = the built-in curated
    /// safe subset. A non-empty list is INTERSECTED with the curated safe set
    /// (so it can only narrow, never reach an outward/cloud tool).
    pub subset: Vec<String>,
}

impl Default for LocalToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rounds: 3,
            subset: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CloudConfig {
    pub fast_model: String,
    pub heavy_model: String,
    pub max_tokens: u32,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            fast_model: "claude-haiku-4-5".to_string(),
            heavy_model: "claude-opus-4-8".to_string(),
            max_tokens: 4096,
        }
    }
}

/// [speech] — neural TTS via the inference server's "speak" op. The daemon
/// passes `voice`, maps opener WAV indices back to `openers` text, and paces
/// clips with `sentence_pause_ms`; `engine` and `speed` are consumed
/// server-side but are mirrored here so the Default impl stays in lockstep
/// with darwin.toml. `instant_opener` (SHIPS ON, full-power default) gates the
/// canned instant acknowledgment: a task-ack WAV plays the instant an utterance
/// ends while STT runs concurrently. Pure UX, no safety surface (set false for
/// warmer, varied greetings instead).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SpeechConfig {
    #[allow(dead_code)] // shared contract; read by the inference server
    pub engine: String,
    /// Explicit HF repo for the engine; "" = the engine's default repo
    /// (resolved server-side from its engine registry).
    #[allow(dead_code)] // shared contract; read by the inference server
    pub model: String,
    pub voice: String,
    #[allow(dead_code)] // shared contract; read by the inference server
    pub speed: f64,
    /// Instant-acknowledgment lines. The server pre-synthesizes each entry to
    /// state/openers/opener-<idx>.wav at startup; the daemon plays one at
    /// utterance end and uses this list (by filename index) to tell the
    /// server which text already went out aloud (opener_spoken).
    pub openers: Vec<String>,
    /// Pure silence inserted between consecutive clips of one reply (after
    /// the opener and between content sentences; never after the last).
    pub sentence_pause_ms: u64,
    /// Opener breath: how long the daemon waits after an utterance ends
    /// before the instant acknowledgment fires. Runs CONCURRENTLY with
    /// transcription (never serialized in front of STT); first_audio_ms
    /// includes it naturally. Only consulted when `instant_opener` is true.
    pub opener_delay_ms: u64,
    /// Master gate for the instant acknowledgment. SHIPS ON (full-power
    /// default): ReplySession::begin breathes `opener_delay_ms`, plays one
    /// `openers` clip the instant an utterance ends (STT runs concurrently),
    /// and passes opener_spoken to converse so the model continues from it.
    /// Pure UX feature, no safety surface. When false the converse stream IS
    /// the whole reply (DARWIN greets/answers naturally from its first word —
    /// some owners prefer that warmer behavior). All the opener machinery stays
    /// intact either way; this flag only decides whether it engages.
    pub instant_opener: bool,
}

impl Default for SpeechConfig {
    fn default() -> Self {
        Self {
            engine: "kokoro".to_string(),
            model: String::new(),
            voice: "bm_george".to_string(),
            speed: 1.2,
            openers: [
                "Right away, sir.",
                "Of course.",
                "One moment.",
                "On it, sir.",
                "Let me see.",
            ]
            .map(String::from)
            .to_vec(),
            sentence_pause_ms: 250,
            opener_delay_ms: 300,
            // SHIPS ON (full-power default). Pure UX feature: plays a task-ack WAV
            // the instant an utterance ends while STT runs concurrently — no safety
            // surface. (Owner tradeoff: some prefer it OFF for warmer, varied
            // greetings instead of a canned acknowledgment; set false to restore
            // that. All the opener machinery stays intact either way.)
            //
            // SHIPS OFF (owner preference): the canned "Right away, sir." ack is off
            // by default so the persona greets/answers naturally from its first word
            // (no robotic fixed opener). Set true to bring the instant ack back.
            instant_opener: false,
        }
    }
}

/// [voice] — the OPTIONAL ElevenLabs cloud VOICE TIER (voice_tier.rs). An ADDED
/// premium-TTS layer on top of the on-device Kokoro default ([speech].voice),
/// NEVER a replacement. SHIPS ON (full-power default) but INERT WITHOUT A KEY:
///
///   - `cloud_tier` (SHIPS ON): the master switch. INERT WITHOUT A KEY — the
///     ElevenLabs path is reached ONLY when this is true AND an `elevenlabs_api_key`
///     is present in the Keychain AND the runtime model-swap tier is non-Local;
///     otherwise (no key, OR offline/"work offline") TTS is EXACTLY today's
///     on-device Kokoro (the private default + the fallback on any EL error).
///     Honesty: when the tier is ACTIVE, the text to synthesize LEAVES the device
///     (a cloud round trip to api.elevenlabs.io).
///   - `cloud_stt` (SHIPS ON; build 2/2): the SEPARATE master switch for the
///     ElevenLabs Scribe cloud-STT tier. Gated independently of `cloud_tier` on
///     purpose — STT sends the user's VOICE AUDIO to the cloud, which is MORE
///     sensitive than the TTS text leg. INERT WITHOUT A KEY: needs the EL key + a
///     non-Local tier; otherwise transcription is EXACTLY today's on-device
///     mlx_whisper (the private default + the fallback on ANY Scribe error/offline).
///     Honesty: when ACTIVE, the VOICE AUDIO leaves the device.
///   - `model` (default "eleven_flash_v2_5"): the ElevenLabs model id. Read by the
///     inference server when it makes the (credential+runtime-gated) TTS call.
///   - `voices` (default empty): a per-agent map, agent name -> ElevenLabs voice id.
///     An empty map or an unmapped agent falls back to that agent's Kokoro voice —
///     so turning the tier on with no mapping still works (every agent just keeps
///     its on-device voice until the operator maps it). VOICE-ONLY: this is a TTS
///     voice layer; DARWIN owns its own brain/router/turn-taking.
///   - `mic_source` (default "device"): where capture frames come FROM. "device"
///     (the default) is today's cpal path, byte-for-byte unchanged. "app" routes
///     the mic IN over a confined Unix socket (`state/ipc/audio_in.sock`, 0700 dir
///     / 0600 socket) from the HUD: the daemon reads a token-authenticated
///     handshake (the SAME per-boot command token, `apps::verify_command_token`),
///     then ingests length-prefixed f32 frames into the SAME VAD/barge/lockdown/
///     meter pipeline — only the SOURCE of frames changes. Any other value is
///     treated as "device".
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// Master switch for the ElevenLabs cloud voice tier (TTS). SHIPS ON (full-power
    /// default) — INERT WITHOUT A KEY: reached only when true AND an
    /// `elevenlabs_api_key` is in the Keychain AND the tier is non-Local; otherwise
    /// TTS is today's on-device Kokoro. An ADDED tier; when active the TTS text leaves
    /// the device.
    pub cloud_tier: bool,
    /// Master switch for the ElevenLabs Scribe cloud-STT tier (build 2/2). SHIPS ON
    /// (full-power default) and is GATED INDEPENDENTLY of `cloud_tier`: STT sends the
    /// user's VOICE AUDIO to the cloud — MORE sensitive than the TTS text leg — so it
    /// has its own switch. INERT WITHOUT A KEY: needs the EL key + a non-Local tier;
    /// otherwise transcription is today's on-device mlx_whisper (also the fallback on
    /// ANY cloud error). When active the voice audio leaves the device.
    pub cloud_stt: bool,
    /// The ElevenLabs model id used when the tier is active. Read server-side.
    pub model: String,
    /// Per-agent ElevenLabs voice ids (agent name -> EL voice id). Empty/unmapped
    /// -> that agent's Kokoro voice (the fallback). BTreeMap for deterministic
    /// iteration (stable tests / telemetry).
    pub voices: std::collections::BTreeMap<String, String>,
    /// #33 ADAPTIVE TONE / PROSODY (prosody.rs). SHIPS ON (full-power default). A
    /// PURE context->profile classifier picks a ProsodyProfile
    /// (Neutral|Calm|Urgent|Warm) and `shape_speak_request` emits ElevenLabs v3
    /// audio-tags + stability/style values ONLY when the resolved backend is
    /// ElevenLabs AND its model is v3-capable; on Kokoro (and non-v3 EL models) the
    /// mapping is COARSE / neutral — rich prosody is EL-v3-GATED and that limitation
    /// is stated honestly, NEVER faked. EXPRESSIVENESS-ONLY: changes delivery, never
    /// a gate/policy/autonomy surface.
    pub adaptive_prosody: bool,
    /// #34 WHISPER / DISCREET MODE (prosody.rs). SHIPS ON (full-power default). An
    /// EXPLICIT command ("whisper mode" / "speak quietly" / "back to normal") toggles
    /// a terse + SOFT (low-volume) delivery. Whisper changes DELIVERY ONLY — it NEVER
    /// suppresses a safety confirmation the gate requires (a required confirm still
    /// speaks, just softly/tersely).
    pub whisper: bool,
    /// #34 OPTIONAL auto-engage of whisper mode by SUSTAINED low-amplitude input — a
    /// PURE energy-series heuristic. SHIPS ON (full-power default) and gated
    /// SEPARATELY from `whisper`: it does NOT open the mic here; it is a pure function
    /// over an energy series the audio layer already computes. Delivery-only.
    pub whisper_auto: bool,
    /// #31 MULTI-SPEAKER DIARIZATION (diarize.rs). SHIPS ON (full-power default) —
    /// INERT ON-DEVICE: a PURE mapper CONSUMES the speaker labels the ElevenLabs
    /// SCRIBE STT backend reports (it carries per-word/segment speaker ids) into a
    /// diarized transcript. On-device whisper has NO diarization model, so the
    /// on-device path is an HONEST single-stream "speaker: unknown" labeling — it
    /// NEVER fabricates distinct speakers the backend did not report. Diarization is
    /// EL-Scribe-gated; that limitation is stated honestly, never faked.
    pub diarize: bool,
    /// SOUND-EFFECT CUE TIER. SHIPS ON (full-power default) — INERT WITHOUT A KEY:
    /// gates the inference server's `sound_effect` op (text prompt -> a short
    /// generated SFX cue), reached only when an `elevenlabs_api_key` is in the
    /// Keychain AND the tier is non-Local; otherwise NO cue is produced (silent
    /// no-op — there is no on-device SFX generator, stated honestly, never faked).
    /// An ADDED cue layer reached only through its explicit gate/command — it NEVER
    /// changes the default speech path. When active the SFX text prompt leaves the
    /// device (text only). Mirrors cloud_tier's credential+runtime gating.
    pub cloud_sfx: bool,
    /// MUSIC GENERATION TIER. SHIPS ON (full-power default) — INERT WITHOUT A KEY:
    /// gates the inference server's `compose_music` op (a text prompt -> a generated
    /// full-length music track WAV), reached only when an `elevenlabs_api_key` is in
    /// the Keychain AND the tier is non-Local; otherwise NO track is produced (honest
    /// unavailable — there is no on-device music generator, stated honestly, never
    /// faked). An ADDED generation layer reached only through its explicit
    /// gate/command — it NEVER changes the default speech path. When active the music
    /// text prompt leaves the device (text only). Mirrors cloud_sfx's
    /// credential+runtime gating.
    pub cloud_music: bool,
    /// LOW-LATENCY STREAMING TTS. OPT-IN (ships OFF) — default behavior is unchanged
    /// (today's blocking synthesis). When true AND the resolved backend is ElevenLabs,
    /// `speak` requests the streaming endpoint for first-audio latency; the inference
    /// server FALLS BACK to blocking on any streaming error, so a turn is never failed
    /// by the streaming leg. Inert on Kokoro (on-device path is unchanged). Delivery
    /// timing only — never a gate/policy surface.
    pub stream_tts: bool,
    /// The ACTIVE ElevenLabs pronunciation-dictionary id threaded as a `speak`
    /// pronunciation locator (the `create_pronunciation` op returns these ids).
    /// DEFAULT "" (none): empty = no locator is sent, so speech is byte-for-byte
    /// today's. Non-empty = the server adds this dictionary to the speak request's
    /// pronunciation_locators. EL-pronunciation-gated; inert on Kokoro.
    pub pronunciation_dictionary_id: String,
    /// OPTIONAL version for the active pronunciation-dictionary locator above.
    /// DEFAULT "" (none): empty = the latest version is used (no version pinned);
    /// non-empty = this exact version_id is sent alongside the dictionary id. Inert
    /// when `pronunciation_dictionary_id` is empty.
    pub pronunciation_dictionary_version: String,
    /// EVENT CUES. OPT-IN (ships OFF) so DEFAULT BEHAVIOR IS UNCHANGED. When true,
    /// a short ElevenLabs SFX cue is FIRE-AND-FORGOTTEN on a couple of key system
    /// events (a `confirm` plays "success", a `deny` plays "notify"). The cue is
    /// purely cosmetic feedback: it is spawned detached AFTER the underlying action
    /// has completed, so it NEVER blocks, delays, or changes the outcome/reply of
    /// the confirm/deny, and a cue error is swallowed. It rides the SAME gate as the
    /// SFX cue tier (`cloud_sfx` + an `elevenlabs_api_key` in the Keychain + a
    /// non-Local tier); without that it is a silent no-op. Default off => ZERO
    /// behavior change.
    pub event_cues: bool,
    /// MICROPHONE SOURCE. DEFAULT "device" (today's behavior, byte-for-byte): the
    /// daemon opens the local input device with cpal and captures it. Set "app" to
    /// route microphone audio IN over a confined Unix socket from the HUD app
    /// instead of the daemon opening the mic itself — the daemon binds
    /// `state/ipc/audio_in.sock` (0700 dir, 0600 socket), reads a token-authenticated
    /// handshake (the SAME per-boot command token, verified with
    /// `apps::verify_command_token`), then ingests length-prefixed f32 frames into
    /// the SAME capture-processing pipeline (VAD / barge / lockdown / meter all
    /// identical — only the SOURCE of frames changes). Any value other than "app"
    /// is treated as "device" (the safe default). The socket path is local-only and
    /// token-gated; an invalid token closes the connection with no audio ingested.
    pub mic_source: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default) — INERT WITHOUT A KEY: the ElevenLabs
            // TTS tier is reached only when cloud_tier=true AND elevenlabs_api_key
            // is in the Keychain AND the runtime tier is non-Local; otherwise
            // on-device Kokoro (the private default + fallback on any EL error).
            // Honesty: when active, the TTS TEXT leaves the device.
            cloud_tier: true,
            // SHIPS ON (full-power default) — INERT WITHOUT A KEY, separately gated
            // from cloud_tier on purpose: STT sends VOICE AUDIO (more sensitive than
            // TTS text). Needs the EL key + a non-Local tier; otherwise on-device
            // mlx_whisper (the private default + fallback). Honesty: when active, the
            // VOICE AUDIO leaves the device.
            cloud_stt: true,
            model: "eleven_flash_v2_5".to_string(),
            // Default per-agent ElevenLabs voice: Darwin-Prime -> "George", an
            // ElevenLabs PREMADE (shared, stable) British male voice available to
            // ANY account. So once an EL key is in the Keychain the cloud voice
            // engages with NO manual voice-id mapping (the formerly-empty map was
            // the silent reason the EL tier never engaged). Other agents stay on
            // their on-device Kokoro voice until mapped here. Override to taste.
            voices: [("darwin".to_string(), "JBFqnCBsd6RMkjVDRZzb".to_string())]
                .into_iter()
                .collect(),
            // #33 SHIPS ON (full-power default). Expressiveness-only: picks a
            // ProsodyProfile and emits EL-v3 audio-tags ONLY on EL-v3 backends; on
            // Kokoro (and non-v3 EL models) the mapping is coarse/neutral (rich
            // prosody is EL-v3-gated, stated honestly). Changes delivery, never a gate.
            adaptive_prosody: true,
            // #34 SHIPS ON (full-power default). Whisper/discreet mode changes
            // DELIVERY ONLY (terse + soft on an explicit command) — it NEVER
            // suppresses a required safety confirmation (a required confirm still
            // speaks, softly).
            whisper: true,
            // #34 SHIPS ON (full-power default). Auto-engage by sustained
            // low-amplitude input is a PURE energy-series heuristic; it does NOT open
            // the mic itself. Delivery-only.
            whisper_auto: true,
            // #31 SHIPS ON (full-power default) — INERT ON-DEVICE: diarization
            // consumes the speaker labels the EL SCRIBE STT backend reports; on-device
            // whisper has no diarization model => honest single-stream
            // "speaker: unknown" (never fabricated speakers). EL-Scribe-gated.
            diarize: true,
            // SHIPS ON (full-power default) — INERT WITHOUT A KEY, exactly like
            // cloud_tier: the sound_effect cue op is reached only when cloud_sfx=true
            // AND elevenlabs_api_key is in the Keychain AND the tier is non-Local;
            // otherwise NO cue (silent no-op — no on-device SFX generator). An ADDED
            // cue tier reached only through its explicit gate; never changes the
            // default speech path. When active the SFX text prompt leaves the device.
            cloud_sfx: true,
            // SHIPS ON (full-power default) — INERT WITHOUT A KEY, exactly like
            // cloud_sfx: the compose_music op is reached only when cloud_music=true
            // AND elevenlabs_api_key is in the Keychain AND the tier is non-Local;
            // otherwise NO track (honest unavailable — no on-device music generator).
            // An ADDED generation tier reached only through its explicit gate; never
            // changes the default speech path. When active the music text prompt
            // leaves the device (text only) and it generates a full track.
            cloud_music: true,
            // OPT-IN (ships OFF) so DEFAULT BEHAVIOR IS UNCHANGED: streaming TTS is
            // requested only when stream_tts=true AND the backend is ElevenLabs; the
            // server falls back to blocking on any streaming error. Inert on Kokoro.
            // Delivery timing only — never a gate.
            stream_tts: false,
            // DEFAULT "" (none): no pronunciation locator is threaded into speak, so
            // speech is byte-for-byte today's until an operator sets the active
            // dictionary id (returned by the create_pronunciation op). EL-gated.
            pronunciation_dictionary_id: String::new(),
            // DEFAULT "" (none): no version pinned — the latest dictionary version is
            // used. Inert when pronunciation_dictionary_id is empty.
            pronunciation_dictionary_version: String::new(),
            // OPT-IN (ships OFF) so DEFAULT BEHAVIOR IS UNCHANGED: with it false NO
            // event cue is ever spawned, so confirm/deny behave byte-for-byte as
            // today. Even when true the cue is fire-and-forget cosmetic feedback
            // that rides the cloud_sfx gate (no key => silent no-op) and can never
            // affect the action's outcome or timing.
            event_cues: false,
            // DEFAULT "device" (today's behavior, byte-for-byte): the daemon opens
            // the local input device with cpal. "app" instead routes the mic in over
            // state/ipc/audio_in.sock from the HUD (token-authenticated handshake +
            // length-prefixed f32 frames into the SAME capture pipeline). Any other
            // value is treated as "device" (the safe default).
            mic_source: "device".to_string(),
        }
    }
}

/// [wake] — CUSTOM WAKE-WORD (#32, wake.rs). The configured phrase that gates "is this
/// utterance for DARWIN". SHIPS ON (full-power default) + defaults to "darwin" so the
/// default preserves today's activation behavior exactly; the PURE matcher is conservative
/// (case/punct/whitespace-insensitive + a small edit-distance tolerance; never matches an
/// empty/blank phrase; never triggers on a substring of a larger unrelated word). The
/// always-listening loop that consults the matcher is DEVICE-GATED (mic/TCC).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WakeConfig {
    /// Master switch for custom-wake-word gating. SHIPS ON (full-power default): since the
    /// phrase is "darwin", behavior is identical to today unless the phrase is changed.
    pub enabled: bool,
    /// The wake phrase that gates activation. Defaults to "darwin" so even when `enabled`
    /// is flipped on with no override, the default phrase preserves today's wake behavior.
    /// An empty/blank phrase NEVER matches (the matcher rejects it — fail-safe).
    pub phrase: String,
}

impl Default for WakeConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default). The phrase defaults to "darwin", so
            // enabling preserves today's wake behavior exactly (behavior is identical
            // unless the phrase is changed). The always-listening loop that consults
            // the matcher is DEVICE-GATED (needs mic/TCC consent).
            enabled: true,
            // Default phrase preserves today's behavior when the feature is turned on.
            phrase: "darwin".to_string(),
        }
    }
}

/// [interpret] — CONTINUOUS LIVE INTERPRETATION (#30, interpret.rs). When `live` is ON the
/// DEVICE-GATED mic loop feeds each VAD segment through the PURE interpret_segment pipeline
/// (transcribe -> on-device-LLM translate -> render/optionally speak); offline/unavailable
/// degrades HONESTLY (never a fabricated translation). SHIPS ON (full-power default) but
/// INERT WITHOUT TCC/MIC — without Microphone consent the device-gated loop interprets
/// nothing.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InterpretConfig {
    /// Master switch for the continuous live-interpret mode. SHIPS ON (full-power default)
    /// — INERT WITHOUT TCC/MIC: the per-segment interpret pipeline runs from the device-
    /// gated mic loop, so it captures nothing without Microphone consent. Quality is bounded
    /// by the local ~4B model; offline degrades honestly.
    pub live: bool,
    /// Whether the rendered translation is also VOICED (through the single echo-safe speech
    /// path) in addition to being shown. SHIPS OFF (false): render-only by default.
    pub speak: bool,
    /// The SOURCE language to interpret FROM. Empty (the default) => auto-detect (the
    /// translator is told the source is unknown — Babel never claims to KNOW a source it
    /// only guessed).
    pub source_lang: String,
    /// The TARGET language to interpret INTO. Defaults to "English". An empty target is an
    /// honest "which language?" — never a fabricated rendering.
    pub target_lang: String,
}

impl Default for InterpretConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default) — INERT WITHOUT TCC/MIC: the per-segment
            // interpret pipeline runs from the DEVICE-GATED mic loop, so without
            // Microphone consent (macOS TCC) it interprets nothing. Translation
            // quality is bounded by the local ~4B model; offline degrades honestly
            // (never a fabricated translation).
            live: true,
            // Render-only by default — voicing the translation stays its OWN opt-in.
            speak: false,
            // Empty => auto-detect the source language (honest; never claimed-known).
            source_lang: String::new(),
            // A sensible default target so a turned-on interpreter has somewhere to go.
            target_lang: "English".to_string(),
        }
    }
}

/// [inference] — server-side knobs mirrored for the shared contract.
///
/// SPECULATIVE DECODING (#37) joins `preload` and SELECTABLE QUANTIZATION (#39)
/// as PERF/RUNTIME knobs:
///   - `speculative` (SHIPS ON, full-power default): the master gate for draft/
///     speculative decoding in the inference server's generate path. INERT WITHOUT
///     A DRAFT MODEL — it ALSO requires a loadable `draft_model` (ships empty);
///     absent that the server honestly falls back to NORMAL generation and reports
///     `speculative=false` (never faked). The real speedup is device/model-dependent
///     and is NEVER measured headlessly.
///   - `draft_model` (ships ""): the small DRAFT checkpoint mlx_lm uses to
///     propose tokens the main model verifies. Empty => speculative is inert
///     even though `speculative=true` (honest: no draft, normal gen). Set a real
///     small checkpoint to engage.
///   - `quant` (ships "auto"): the requested on-device weight quantization for
///     the LOCAL model load. "auto" == today's behavior (load the model as
///     configured). An explicit value (fp16/int8/int4) asks the server to load
///     a matching quant variant; if that variant is not present the server
///     loads the available one and reports the quant that ACTUALLY loaded — it
///     never claims int4 when fp16 loaded. Validated below; an unknown value is
///     a parse issue and falls back to "auto" (today's behavior).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InferenceConfig {
    #[allow(dead_code)] // shared contract; read by the inference server
    pub preload: bool,
    /// SPECULATIVE/DRAFT decoding master gate (#37). SHIPS ON (full-power default)
    /// — INERT WITHOUT a loadable `draft_model` (=> normal gen + speculative=false
    /// reported, never faked). Read by the inference server's generate path AND by
    /// the daemon's `should_use_speculative` decision / HUD telemetry. The actual
    /// speedup is device/model-gated, never claimed headlessly.
    #[allow(dead_code)] // shared contract; read by the inference server + telemetry
    pub speculative: bool,
    /// Small DRAFT model id mlx_lm uses to propose tokens (#37). "" (default) =>
    /// no draft, so speculative is inert even when `speculative=true` (honest
    /// fallback to normal gen). A non-empty id is the checkpoint the server
    /// lazy-loads; if it cannot load, the server falls back to normal gen and
    /// reports `speculative=false`.
    #[allow(dead_code)] // shared contract; read by the inference server
    pub draft_model: String,
    /// SELECTABLE weight QUANTIZATION for the local model load (#39). "auto"
    /// (default) == today's behavior. Allowed: auto/fp16/int8/int4 (validated by
    /// `InferenceConfig::quant_is_valid`; an unknown value is reported as a
    /// config issue and kept at "auto"). The real RAM/speed/quality tradeoff is
    /// device-gated; the server reports the quant that ACTUALLY loaded.
    #[allow(dead_code)] // shared contract; read by the inference server
    pub quant: String,
}

impl InferenceConfig {
    /// The quantization values the contract allows. MUST match server.py's
    /// `ALLOWED_QUANT` / `validate_quant`. "auto" is the neutral default
    /// (today's behavior — load the model as configured, no quant override).
    pub const ALLOWED_QUANT: &'static [&'static str] = &["auto", "fp16", "int8", "int4"];

    /// Whether `q` is an allowed quantization value (PURE; mirrors the server's
    /// `validate_quant`). Used by the parse-time validation so an unknown value
    /// is reported and kept at the neutral "auto" default rather than passed to
    /// the server.
    pub fn quant_is_valid(q: &str) -> bool {
        Self::ALLOWED_QUANT.contains(&q)
    }
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            preload: true,
            // SHIPS ON (full-power default) — INERT WITHOUT A DRAFT MODEL: speculative
            // decoding also needs a loadable `draft_model` (ships empty); absent that,
            // generate falls back to normal gen and HONESTLY reports speculative=false
            // (never faked). Set draft_model to a real small checkpoint to engage.
            speculative: true,
            // SHIPS EMPTY — INERT-UNTIL-MODEL companion to speculative=true. Set e.g.
            // "mlx-community/Qwen3-0.6B-4bit" (must be downloadable) to
            // actually engage speculative; unloadable => normal gen + speculative=false.
            draft_model: String::new(),
            // "auto" == today's behavior (load the model as configured); an
            // explicit quant is opt-in and device-gated.
            quant: "auto".to_string(),
        }
    }
}

/// [power] — BATTERY/THERMAL ADAPTIVE THROTTLING (#38). PERF/RUNTIME ONLY: this
/// never adds an outward surface, never loosens a gate, never makes a cloud call.
/// It only influences the LOCAL model-tier sub-choice (prefer the cheaper Fast
/// sub-tier + defer heavy work) when the machine is on a low battery or under
/// serious thermal pressure.
///
///   - `adaptive` (SHIPS ON, full-power default): the master gate. PERF-ONLY — the
///     conservative policy may prefer the Fast local sub-tier + defer heavy work on
///     low battery (discharging) or serious/critical thermal pressure; it never
///     loosens a gate and never makes a cloud call. The LIVE power read
///     (pmset/thermal/IOKit) is DEVICE-GATED behind this flag.
///   - `low_battery_pct` (default 20): the discharge threshold below which, ON
///     BATTERY, the conservative policy prefers the Fast local sub-tier + defers
///     heavy work. On AC + nominal thermal the policy never throttles regardless.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PowerConfig {
    pub adaptive: bool,
    pub low_battery_pct: u8,
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default). PERF-ONLY: influences only the LOCAL
            // model sub-choice (prefer cheaper Fast + defer heavy on low battery /
            // serious thermal). Never loosens a gate, never makes a cloud call. The
            // live pmset/thermal read is device-gated behind this flag.
            adaptive: true,
            // Conservative discharge threshold: below 20% on battery, prefer the
            // cheaper local Fast sub-tier + defer heavy work.
            low_battery_pct: 20,
        }
    }
}

/// [report] — REPORT GENERATION (#40, report.rs). `enabled` SHIPS ON (full-power
/// default). The op is READ-ONLY — it pulls the agent-scoped, already-cited
/// notebook/research material and folds it into a BOUNDED markdown report under
/// research.rs's cite discipline (every citation a REAL source ref an input claim
/// carried; an uncited claim DROPPED, never fabricated a source; no citable source
/// -> an HONEST-EMPTY report). It speaks/displays, acts/reaches nothing outward —
/// safe to enable outright.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReportConfig {
    pub enabled: bool,
}

impl Default for ReportConfig {
    fn default() -> Self {
        // SHIPS ON (full-power default). Read-only: folds already-cited
        // notebook/research material into one bounded markdown report under
        // research.rs cite discipline; honest-empty when no citable source.
        // Speaks/displays only, reaches nothing — safe to enable outright.
        Self { enabled: true }
    }
}

/// [chart] — DATA -> CHART (#41, chart.rs). `enabled` SHIPS ON (full-power
/// default). The op is a NEUTRAL presentation act — it serializes a ChartSpec (the
/// EXACT data points) as a `chart.data` telemetry envelope the HUD plots exactly
/// (no interpolation, no invented/extrapolated point, honest axes + honest-empty).
/// It changes no gate, takes no action, reaches no network — safe to enable
/// outright; the emit is fire-and-forget like every other telemetry envelope.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ChartConfig {
    pub enabled: bool,
}

impl Default for ChartConfig {
    fn default() -> Self {
        // SHIPS ON (full-power default). Neutral presentation: serializes the EXACT
        // data points as a chart.data telemetry envelope the HUD plots verbatim (no
        // interpolation/invented point, honest-empty). Changes no gate, takes no
        // action, reaches no network — safe to enable outright.
        Self { enabled: true }
    }
}

/// [artifact] — ARTIFACT REGISTRY + PEEK (artifact.rs). `enabled` SHIPS ON
/// (armed-by-default): producers register the last N things they made (report /
/// chart / code_diff / …) into a BOUNDED, in-memory, on-device recency window, each
/// carrying an HONEST provenance (the real producing agent + the real citations, or
/// UNCITED). The read-only `peek` surface (a voice op + the `artifact_peek` tool)
/// reads the most recent (or an id) back out as an `artifact.peek` telemetry frame
/// the HUD's QuickLook overlay renders. It opens NO outward surface, takes NO
/// action, reaches NO network — it only remembers + shows what was already
/// produced. `registry_size` is the retention bound (kept last-N; clamped to the
/// module ceiling).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ArtifactConfig {
    /// Master gate for the registry + peek surface. SHIPS ON (armed-by-default):
    /// with it off, producers register nothing and peek finds nothing. Read-only
    /// accountability — arming it loosens nothing.
    pub enabled: bool,
    /// Retention bound — the registry keeps this many most-recent artifacts, then
    /// evicts the oldest. Clamped into `[1, artifact::MAX_REGISTRY_BOUND]`.
    pub registry_size: usize,
}

impl Default for ArtifactConfig {
    fn default() -> Self {
        // Armed by default with the module's recency-window bound. Read-only,
        // on-device, opens no outward surface — safe to arm outright.
        Self {
            enabled: true,
            registry_size: crate::artifact::DEFAULT_REGISTRY_BOUND,
        }
    }
}

/// [boundary] — CUSTOMS // EGRESS, the pre-flight egress boundary gate
/// (boundary.rs). CUSTOMS INSPECTS + (reduce-only) TRIMS the personal context a
/// CLOUD turn is about to send — it never mutates state, never sends anything, and
/// never touches the LOCAL inference path (which egresses nothing off the box, so
/// there is nothing for CUSTOMS to gate there).
///
/// `enabled` SHIPS ON (full-power default) as a NEUTRAL PREVIEW: with it on the
/// cloud path builds a READ-ONLY [`crate::boundary::EgressManifest`] of exactly
/// what egresses (facts / history / world rows / persona / system prompt, each
/// classified by sensitivity with a count + byte-size) and emits it as a
/// `boundary.manifest` frame BEFORE the request leaves. With it off the manifest
/// path is skipped and the cloud turn is byte-for-byte today's.
///
/// `default_trim` SHIPS "none" — the IDENTITY: send everything, today's behavior
/// byte-for-byte. A trim is REDUCE-ONLY (`crate::boundary::TrimSpec`): it can only
/// WITHHOLD whole categories from egress, never add one. Recognized values:
///
///   * "none"      — withhold nothing (the shipped identity);
///   * "no_facts"  — withhold the remembered FACTS ("don't send my facts");
///   * "no_memory" — withhold FACTS and conversation HISTORY.
///
/// An unknown value degrades to "none" (a trim must be EXPLICIT — CUSTOMS never
/// silently withholds context the operator did not clearly ask to drop). A
/// per-turn voice command can override this for a single turn without changing the
/// default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BoundaryConfig {
    /// Master gate for the CUSTOMS pre-flight manifest. SHIPS ON (full-power
    /// default) as a neutral READ-ONLY preview of cloud egress. Off => the cloud
    /// turn is byte-for-byte today's (no manifest computed/emitted).
    pub enabled: bool,
    /// The default REDUCE-ONLY trim applied to every cloud turn. SHIPS "none" (the
    /// identity — send everything). "no_facts" withholds the remembered facts;
    /// "no_memory" withholds facts + history; an unknown value degrades to "none".
    /// Parsed by `crate::boundary::TrimSpec::from_str`.
    pub default_trim: String,
}

impl Default for BoundaryConfig {
    /// Ships NEUTRAL: the preview is ON (read-only inventory of cloud egress) and
    /// the trim is "none" (the identity — the turn sends exactly what it sends
    /// today). CUSTOMS observes + reports until the operator opts into a trim.
    fn default() -> Self {
        Self {
            enabled: true,
            default_trim: "none".to_string(),
        }
    }
}

/// [vault] — VAULT MODE ("go dark", vault.rs), a one-word forcing switch that keeps
/// a turn LOCAL-ONLY for sensitive work. Where CUSTOMS ([boundary]) inventories +
/// trims the cloud egress, Vault removes the cloud turn ALTOGETHER: with it active
/// the router refuses to escalate to the Anthropic fallback (the turn stays on the
/// local MLX brain, or honestly says it can't do this offline) and CUSTOMS is forced
/// to its strongest reduce-only trim.
///
/// `enabled` SHIPS OFF: vault CHANGES BEHAVIOR (it removes cloud access), so it is
/// opt-in and never engages silently. It is toggled at runtime — a `vault` router op
/// or a spoken "go dark" / "vault mode on|off" — and this key is only the boot
/// default. RESTRICT-ONLY: vault can only ever REMOVE cloud + tighten the egress
/// trim, never add either — a turn under vault egresses nothing the non-vault turn
/// wouldn't, and with it off the cloud decision is byte-for-byte today's.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VaultConfig {
    /// The boot default for vault mode. SHIPS OFF (vault removes cloud access, so it
    /// is opt-in). Runtime toggles ("go dark" / the `vault` op) flip the live mode;
    /// this only sets the state at startup.
    pub enabled: bool,
}

impl Default for VaultConfig {
    /// Ships OFF — vault changes behavior (local-only + maximal egress trim), so it
    /// never engages until the operator opts in (config default or a runtime toggle).
    fn default() -> Self {
        Self { enabled: false }
    }
}

/// [egress] — EGRESS BASELINE + BEACON DETECTOR (egress_beacon.rs), the
/// longitudinal follow-on to the read-only Egress Sentinel. `enabled` SHIPS ON:
/// like `[introspect]`/`[audit]` it is pure READ-ONLY observability — it samples
/// the SAME lsof outbound snapshot, keeps a BOUNDED baseline, and runs two PURE
/// classifiers (first-seen talker + regular-interval beacon cadence). Alerts RIDE
/// EDITH's quiet-hours (inherited from `[proactive]`) + cooldown + debounce so
/// they never spam, and any "block" is PROPOSE-ONLY: a pf rule rendered as TEXT
/// the user applies with sudo — the loop never mutates the firewall.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EgressConfig {
    /// Master switch for the sampling loop. SHIPS ON (read-only observability);
    /// with it false the loop is simply not spawned.
    pub enabled: bool,
    /// Seconds to wait after boot before the first sample (let the host settle).
    pub startup_delay_secs: u64,
    /// Seconds between outbound-connection samples. The beacon-cadence resolution
    /// is bounded by this: a beacon that opens AND closes entirely between two
    /// samples is invisible to snapshot sampling (an honest limit, not a bug).
    pub sample_interval_secs: u64,
    /// A talker not seen for longer than this (seconds) is pruned from the
    /// baseline (bounded retention).
    pub retention_secs: u64,
    /// Hard cap on distinct talkers held; the least-recently-seen is evicted when
    /// a new talker would exceed it (bounded memory).
    pub max_talkers: usize,
    /// Ring cap on rising-edge timestamps kept per talker (bounded memory).
    pub max_samples_per_talker: usize,
    /// Minimum rising-edge timestamps before a beacon-cadence verdict is trusted.
    pub beacon_min_samples: usize,
    /// Mean inter-arrival at/above this (seconds) — below it a series is treated
    /// as bursty reconnection noise, not a beacon.
    pub beacon_min_interval_secs: u64,
    /// Mean inter-arrival at/below this (seconds) — above it the cadence is
    /// indistinguishable from ordinary slow polling at our sample resolution.
    pub beacon_max_interval_secs: u64,
    /// Coefficient-of-variation ceiling (stddev/mean of the deltas). A tight,
    /// regular cadence sits well below this; a jittery/random one blows past it.
    pub beacon_max_jitter: f64,
    /// Per-talker alert cooldown (seconds): don't renag on the same host.
    pub alert_cooldown_secs: u64,
    /// Global debounce (seconds): never two egress alerts closer than this.
    pub alert_min_gap_secs: u64,
}

impl Default for EgressConfig {
    fn default() -> Self {
        // SHIPS ON (read-only observability). Conservative retention + beacon/alert
        // thresholds: sample once a minute, keep a day of baseline, flag only tight
        // (CV <= 0.15) regular cadences between 30s and 1h, and gate alerts behind a
        // 6h per-host cooldown + a 5-min global debounce (on top of EDITH quiet hours).
        Self {
            enabled: true,
            startup_delay_secs: 45,
            sample_interval_secs: 60,
            retention_secs: 24 * 60 * 60, // 1 day
            max_talkers: 2048,
            max_samples_per_talker: 64,
            beacon_min_samples: 6,
            beacon_min_interval_secs: 30,
            beacon_max_interval_secs: 60 * 60, // 1 hour
            beacon_max_jitter: 0.15,
            alert_cooldown_secs: 6 * 60 * 60, // 6 hours
            alert_min_gap_secs: 5 * 60,       // 5 minutes
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SelfHealConfig {
    /// Master gate. SHIPS ON (full-power default) — INERT WITHOUT A CLOUD KEY: the
    /// heavy-model unified-diff draft requires ANTHROPIC_API_KEY; with no key the
    /// watchdog emits heal.blocked{reason:"no_api_key"} and patches nothing.
    pub enabled: bool,
    /// "propose" (default): a validated patch is written to
    /// state/heal/proposals/<ts>/ with its report, meta.heal_pending is
    /// stamped, and a human applies it via scripts/apply_heal.sh <ts>.
    /// "auto" (DANGEROUS; additionally requires enabled = true): the daemon
    /// applies the validated patch to daemon/ itself, rebuilds --release,
    /// and EXITS cleanly so its supervisor restarts it — under launchd
    /// KeepAlive that is a restart, under `cargo run` it is a stop.
    /// Unknown values fall back to "propose" (the safe behavior).
    pub mode: String,
}

impl Default for SelfHealConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default) — INERT WITHOUT A CLOUD KEY: the
            // heavy-model unified-diff draft requires the Anthropic cloud key; with
            // no key the watchdog emits heal.blocked{reason:"no_api_key"} and patches
            // nothing. Needs ANTHROPIC_API_KEY to actually draft.
            enabled: true,
            // KEEP "propose" — propose -> human runs scripts/apply_heal.sh IS the
            // gate. "auto" is DANGEROUS (the daemon applies a patch to itself and
            // restarts); NEVER ship "auto" as the default.
            mode: "propose".to_string(),
        }
    }
}

/// [optimize] — the optimization-from-usage loop (optimize.rs). The SAME
/// propose-only contract as [self_heal]/[forge], applied to "learn better
/// routing/selection from how interactions actually went":
///
///   - `enabled` (SHIPS ON, full-power default): master gate. Live trace recording
///     is runtime-gated (traces accrue only while the daemon runs with this on) and
///     PII-redacted before storage; the optimizer still only PROPOSES diffs a human
///     applies.
///   - `mode` ("propose" default; "auto" reserved): the downstream Optimizer
///     phase reuses the self-heal posture — it PROPOSES a measured config/
///     prompt/example diff for human review+apply and NEVER silently mutates a
///     live config. The Trace Store itself acts on neither value; it only ever
///     records (when enabled) and reads. Unknown values fall back to "propose"
///     (the safe behavior).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OptimizeConfig {
    pub enabled: bool,
    pub mode: String,
}

impl Default for OptimizeConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default). Live trace recording is runtime-gated
            // (traces accrue only while the daemon runs with this on); traces are
            // PII-redacted before storage. The optimizer still only PROPOSES diffs a
            // human applies.
            enabled: true,
            // KEEP "propose" — the optimizer writes a measured diff for human
            // review+apply, adopted only if it beats baseline on held-out traces.
            // There is no auto-apply-to-live path; NEVER ship "auto" as the default.
            mode: "propose".to_string(),
        }
    }
}

/// [explain] — CAUSA, the causal decision-trace explainer (explain.rs). A
/// READ-ONLY, ARMED-BY-DEFAULT surface: the turn loop folds each turn's already-
/// computed branch signals (intent / selector mode / agent / local-vs-cloud route
/// / owner gate / capability / outcome) into an ordered, REDACTED [`DecisionTrace`]
/// held in a small bounded ring (last `ring_size` turns). "why did you do that" /
/// "why <Agent>" narrates the trace in persona and emits the secret-free
/// `causa.trace` telemetry — it NEVER fabricates a rationale and returns an honest
/// empty when a turn wasn't recorded. It changes nothing about routing; it only
/// explains what already happened.
///
///   - `enabled` (SHIPS ON, full-power default): master gate for the record pass +
///     the explain op. Off => no trace is recorded and "why did you do that" falls
///     through to the model. Analytics/observability only — no autonomy.
///   - `ring_size`: how many recent turns the ring retains (clamped to
///     [`crate::explain::RING_CAP_MIN`]..=[`crate::explain::RING_CAP_MAX`]); an
///     out-of-band value never disables recording nor grows unbounded.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExplainConfig {
    pub enabled: bool,
    pub ring_size: usize,
}

impl Default for ExplainConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (read-only observability). The trace is redacted at assembly
            // and bounded; the explainer only ever DESCRIBES a past turn — no
            // autonomy, no routing change.
            enabled: true,
            ring_size: crate::explain::RING_CAP_DEFAULT,
        }
    }
}

/// [mirror] — MIRROR, belief-audit + contest over the SELF-MODEL (user_model.rs). A
/// READ-ONLY / REDUCE-ONLY, ARMED-BY-DEFAULT surface. "why do you think I prefer X"
/// surfaces the STORED observation, provenance, and observed-count of that belief
/// (never a fabricated reason); "that's wrong about X" DROPS the belief AND writes a
/// `user.model.suppressed.*` tombstone that the consolidation pass consults so the
/// belief is NEVER silently re-derived (the tombstone is user-clearable). It emits
/// the secret-free `mirror.belief` telemetry frame.
///
///   - `enabled` (SHIPS ON, full-power default): master gate for the explain +
///     contest voice arm. Off => "why do you think…" / "that's wrong…" fall through
///     to the model. Contest is REDUCE-ONLY (removes/suppresses a SHARED belief; it
///     is structurally unable to touch a private `agent.*` note) — no autonomy.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MirrorConfig {
    pub enabled: bool,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        // SHIPS ON — a read-only / reduce-only self-model surface: it only ever
        // EXPLAINS a stored belief or REMOVES + suppresses one at the user's word.
        Self { enabled: true }
    }
}

/// [calibrate] — PLUMBLINE, the confidence-calibration self-report (calibrate.rs).
/// A READ-ONLY fold over the recent (confidence, outcome) window into a reliability
/// curve + a scalar over/under-confidence gap (ECE-style), with a MIN_SAMPLE floor
/// so a thin bucket is reported "insufficient data" rather than judged. It emits the
/// aggregate, secret-free `calibrate.report` telemetry and changes NOTHING on its
/// own.
///
///   - `enabled` (SHIPS ON, full-power default): master gate for the report pass.
///     Analytics only — no PII (a sample is a float + an outcome enum), no autonomy.
///     Off => no report is emitted (the pure math is unaffected).
///   - `influence_routing` (SHIPS OFF): gates the REDUCE-ONLY clarify-band hook
///     ([`crate::calibrate::adjusted_clarify_threshold`]). When on, the router MAY
///     RAISE its clarify/low-confidence threshold in a bucket PLUMBLINE measured as
///     overconfident — asking MORE clarifying questions there. The hook is
///     mathematically incapable of LOWERING the threshold, so it can only ever make
///     DARWIN more cautious, never bolder. Off by default: routing is byte-for-byte
///     today's and PLUMBLINE is pure analytics.
///   - `n_bins` (deciles): reliability resolution. `min_sample`: the per-bucket
///     honesty floor. `overconfidence_margin`: how far actual must lag the claim
///     before a bucket counts as overconfident (dead-band against noise).
///     `max_widen`: the cap on how far the reduce-only hook may raise the threshold.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CalibrateConfig {
    pub enabled: bool,
    pub influence_routing: bool,
    pub n_bins: usize,
    pub min_sample: usize,
    pub overconfidence_margin: f64,
    pub max_widen: f64,
}

impl Default for CalibrateConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON — READ-ONLY aggregate analytics (a reliability curve + ECE
            // gap over the recent confidence/outcome window), the same always-on,
            // no-autonomy posture as the eval scorecard / [episodic].
            enabled: true,
            // SHIPS OFF — the reduce-only clarify-band hook is inert by default so
            // the first landing is pure analytics and routing is unchanged. Even
            // when on, it can ONLY widen the clarify band (ask more), never narrow
            // it (act bolder).
            influence_routing: false,
            // Deciles — fine enough to see a miscalibration bend, coarse enough that
            // a bucket can reach the floor on a real corpus.
            n_bins: crate::calibrate::DEFAULT_BINS,
            // Per-bucket honesty floor: below this many graded turns a bucket is
            // "insufficient data" and excluded from the ECE / gap.
            min_sample: crate::calibrate::DEFAULT_MIN_SAMPLE,
            // A bucket is only "overconfident" if actual success lags the claim by
            // MORE than this — a dead-band so ordinary sampling noise never widens.
            overconfidence_margin: 0.1,
            // Cap on the reduce-only widen (the hook never raises the threshold by
            // more than this, and the result is always capped at 1.0).
            max_widen: 0.15,
        }
    }
}

/// [episodic] — the EPISODIC STORE (episodic.rs): DARWIN's durable, redacted,
/// agent-scoped, BOUNDED memory of completed interactions, and the recall over
/// it. The episodic store ships **ON** — it is the SAME always-on posture as the
/// `transcripts` table and the lifelong-learning fact loop: a per-completed-turn
/// LOCAL record that powers READ-ONLY recall, not
/// any autonomous behavior. The honesty that earns the on-default:
///
///   - `enabled` (ships TRUE, default-on-but-BOUNDED): the master switch. When
///     true, a completed turn is recorded as an episode ONLY through the same
///     gates that already govern transcript/learning recording — a screen-read
///     TRANSIENT turn and a voice-id-UNVERIFIED turn are NEVER recorded, nor is
///     an empty/abandoned turn. Every field is REDACTED before store (reusing the
///     optimize::redact redactor), recall is AGENT-SCOPED (an episode stays in its
///     agent's scope), and retention is BOUNDED (evict-oldest past `retention`).
///     Turn it OFF to record no episodes at all; recall then returns nothing
///     (honest empty), it never fabricates one.
///   - `retention` (episodes_keep): the evict-oldest cap on the on-disk store —
///     the bounded-memory contract. The store remembers the RECENT past, NOT
///     "everything forever"; past the cap the OLDEST episodes are dropped by the
///     same retention pass that caps transcripts.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EpisodicConfig {
    pub enabled: bool,
    pub retention: usize,
}

impl Default for EpisodicConfig {
    fn default() -> Self {
        Self {
            // Ships ON — the same always-on posture as the transcripts table /
            // lifelong-learning fact loop. It is a READ-ONLY record (not an autonomy
            // loop), bounded, redacted, agent-scoped, gated, and forgettable.
            enabled: true,
            // Evict-oldest cap. Generous for a meaningful recent history, small
            // enough that the on-disk store stays tiny on the always-on appliance.
            retention: 5_000,
        }
    }
}

/// [notebooks] — RESEARCH NOTEBOOKS (notebook.rs): the persistent, redacted,
/// agent-scoped, BOUNDED store of SAGE research runs. A run is saved as a CITED
/// notebook entry {topic, synthesized text, the real fetched citations, ts}; the
/// user can REVISIT a notebook and APPEND a follow-up run to it (source memory
/// accrues). Same posture as [episodic]: always-on-but-bounded, NOT an autonomy
/// gate — a notebook is a READ-ONLY persisted record of a research run that
/// already happened, under the SAME cite-discipline research.rs enforces (a
/// notebook holds NO citation that was not in its run, never a fabricated one).
///
///   - `enabled` (ships TRUE, default-on-but-BOUNDED): the master switch. With it
///     false no run is saved and revisit returns an HONEST EMPTY (never fabricates).
///     The synthesized text is redacted before store, scope is agent-scoped, and
///     the store is forgettable (per-notebook or per-agent).
///   - `retention` (entries_keep): the evict-oldest cap on the on-disk store — the
///     bounded-memory contract. The store remembers the recent runs, NOT
///     "everything forever"; past the cap the OLDEST entries (and their citations)
///     are dropped by `memory::notebook_retention_pass`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NotebookConfig {
    pub enabled: bool,
    pub retention: usize,
}

impl Default for NotebookConfig {
    fn default() -> Self {
        Self {
            // Ships ON — same always-on-but-bounded posture as [episodic]. It is
            // bounded, redacted, agent-scoped, cited, and forgettable.
            enabled: true,
            // Evict-oldest ENTRIES cap. A research run is heavier than an episode
            // (a synthesis + a bibliography), so the entry cap is smaller than the
            // episodes cap while still holding a generous recent shelf.
            retention: 500,
        }
    }
}

/// [lifelog] — the LIFE-LOG DIGEST (lifelog.rs): a periodic (daily/weekly)
/// browsable summary built ONLY from the agent-scoped, redacted EPISODIC store.
/// Same posture as [episodic]/[notebooks]: always-on-but-bounded, NOT an autonomy
/// gate — the digest is a READ-ONLY, DETERMINISTIC fold over episodes that already
/// exist (it needs no model/network), and it NEVER fabricates: a window with no
/// episodes yields an HONEST EMPTY digest, a sparse window says exactly what little
/// it holds.
///
///   - `enabled` (ships TRUE, default-on-but-bounded): the master switch. With it
///     false the digest intent returns an honest "the life log is off". The digest
///     owns NO store of its own — its bound is the episodic store's bound, and
///     forgetting episodes empties it.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LifeLogConfig {
    pub enabled: bool,
}

impl Default for LifeLogConfig {
    fn default() -> Self {
        Self {
            // Ships ON — same always-on posture as [episodic]; it is a read-only,
            // never-fabricating fold over the bounded episodic store.
            enabled: true,
        }
    }
}

/// [docsearch] — ON-DEVICE FILE RAG (docsearch.rs): index + cosine/BM25 search
/// over the user's OWN text-like files, 100% on-device. SHIPS ON (full-power
/// default) but INERT WITHOUT ROOTS: because the folder allowlist (`roots`) ships
/// EMPTY and the installer must NOT guess folders, it indexes NOTHING until the
/// user allowlists a folder — it is never a whole-disk scan.
///
///   - `enabled` (SHIPS ON, full-power default): master switch. INERT WITHOUT
///     ROOTS — with an empty `roots` the indexer touches nothing.
///   - `roots` (SHIPS EMPTY): the EXPLICIT allowlist of folders that may be
///     indexed. NEVER a whole-disk scan — even enabled, an empty `roots` indexes
///     nothing. Add absolute folder paths to make anything searchable. Every
///     candidate file is PATH-CONFINED (canonicalize
///     + assert it starts_with a canonicalized allowed root; symlink-escape / `..`
///       / absolute-elsewhere are REJECTED), so the index can never reach a file
///       outside an allowlisted root.
///   - `max_files` / `max_chunks` / `max_file_bytes` / `max_depth` /
///     `chunk_chars` / `chunk_overlap`: the BOUNDS — total files, total chunks,
///     per-file byte cap, recursion depth, chunk window size, and overlap. They
///     keep the on-disk store finite (bounded memory), exactly like the
///     [mcp]/[episodic] bounds. Hidden + binary + non-allowlisted-extension files
///     are skipped regardless.
///
/// HONESTY: file CONTENTS + EMBEDDINGS never leave the device — embedding is the
/// on-device MLX embed op and falls back to lexical BM25 when that server is down
/// (the search reports which actually ran). v1 indexes TEXT-LIKE files only; PDFs
/// and other binaries are OUT OF SCOPE (a PDF needs a parser dependency — they are
/// skipped, never silently "indexed"). The index is FORGETTABLE (clear it).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DocSearchConfig {
    pub enabled: bool,
    pub roots: Vec<String>,
    pub max_files: usize,
    pub max_chunks: usize,
    pub max_file_bytes: usize,
    pub max_depth: usize,
    pub chunk_chars: usize,
    pub chunk_overlap: usize,
    /// KNOWLEDGE GRAPH (knowledge_graph.rs): when true, the "build/map knowledge
    /// graph from my documents" intent (and an OPTIONAL auto-pass after a reindex)
    /// runs the conservative DETERMINISTIC extractor over the already-indexed
    /// chunks and UPSERTs the grounded entities/relationships into the SHARED
    /// `user.world.*` tier (provenance-tagged, deduped, bounded). SHIPS ON
    /// (full-power default) — INERT WITHOUT INDEXED DOCS: the graph build reads only
    /// chunks the confined, allowlisted indexer already produced and writes only the
    /// shared world tier; it never re-walks the disk and never writes an agent's
    /// private namespace. Inert until docsearch has roots + an index.
    pub build_graph: bool,
}

impl Default for DocSearchConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default) — INERT WITHOUT ROOTS: even enabled it
            // indexes nothing until the user allowlists a folder. Contents +
            // embeddings never leave the device; path-confined, bounded, forgettable.
            enabled: true,
            // SHIPS EMPTY — the installer must NOT guess folders. Add absolute folder
            // paths to make anything searchable; nothing else is ever read. An empty
            // allowlist means "index nothing" even with `enabled` true.
            roots: Vec::new(),
            // Generous-but-finite ceilings; the master switch + empty roots are what
            // actually ship the subsystem off.
            max_files: 5_000,
            max_chunks: 50_000,
            // 2 MiB per file — large enough for source/notes, small enough to skip
            // accidental blobs.
            max_file_bytes: 2 * 1024 * 1024,
            // Recursion depth bound on the std::fs walk (root itself is depth 0).
            max_depth: 16,
            // ~1200-char overlapping windows keep a chunk focused yet citeable.
            chunk_chars: 1_200,
            chunk_overlap: 200,
            // SHIPS ON (full-power default) — INERT WITHOUT INDEXED DOCS: the
            // deterministic extractor only reads chunks the confined allowlisted
            // indexer produced (never re-walks disk) and writes the shared
            // user.world.* tier (provenance-tagged, deduped, bounded). Inert until
            // docsearch has roots + an index.
            build_graph: true,
        }
    }
}

/// [code] — CODE INTELLIGENCE (code.rs): the read-only `code_explain` (a grounded,
/// CITED answer over the on-device docsearch code index) + the PROPOSE-ONLY
/// `code_propose_diff` (a reviewable unified diff written to
/// state/code/proposals/<ts>/ — it NEVER edits the user's tree). SHIPS ON
/// (full-power default) but INERT WITHOUT ROOTS: because it READS and PROPOSES
/// EDITS to the user's code, it does NOTHING until the user allowlists a codebase
/// root — the installer must NOT guess one. code_propose_diff drafting also needs
/// the cloud key.
///
///   - `enabled` (SHIPS ON, full-power default): master switch. INERT WITHOUT
///     ROOTS — with an empty `roots` `code_explain`/`code_propose_diff` reach
///     nothing.
///   - `roots` (SHIPS EMPTY): the EXPLICIT allowlist of codebase roots. NEVER an
///     arbitrary path. The human apply script (scripts/apply_code_diff.sh) writes
///     ONLY under a canonicalized root (confined BY CONSTRUCTION via sandbox-exec
///     deny-default-write), and `code_explain` answers only from the docsearch
///     index built over allowlisted roots. An empty allowlist means "no codebase
///     is reachable" even with `enabled` true.
///   - `max_diff_bytes`: the BOUND on a proposed diff's size, so the proposal
///     artifact stays finite (a degenerate/huge model diff is refused).
///
/// HONESTY: `code_explain` is GROUNDED + CITED — it answers ONLY from the real
/// indexed code chunks (file + offset) and never fabricates code that is not in
/// the index (an empty/no-match index => an honest "I don't have that indexed").
/// `code_propose_diff` is PROPOSE-ONLY — a reviewable diff to the proposal store,
/// NEVER an auto-edit; the apply is the human-reviewed, confined-by-construction
/// script. The model's diff QUALITY (does it compile/work) is runtime/model-gated
/// and NOT claimed measured. On-device-first: the code index is on-device; the
/// authoring model is per the active tier.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CodeConfig {
    pub enabled: bool,
    pub roots: Vec<String>,
    pub max_diff_bytes: usize,
}

impl Default for CodeConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default) — INERT WITHOUT ROOTS: code_explain
            // answers only from the on-device index, and the apply script writes ONLY
            // under a canonicalized allowlisted root (sandbox-exec deny-default-write).
            // code_propose_diff is propose-only and its drafting needs the cloud key.
            enabled: true,
            // SHIPS EMPTY — the installer must NOT guess. Add the absolute path to
            // your project; the apply script writes ONLY under a root here (never an
            // arbitrary path). Also allowlist the same root under [docsearch].roots
            // (and reindex) so code_explain can actually retrieve it.
            roots: Vec::new(),
            // 256 KiB — large enough for a substantial multi-file refactor diff,
            // small enough to refuse a degenerate/runaway model output.
            max_diff_bytes: 256 * 1024,
        }
    }
}

/// [shell] — the SANDBOXED SHELL / TERMINAL (#43), the HIGHEST-RISK capability:
/// arbitrary command execution. It SHIPS ON (full-power default) and is maximally
/// gated by construction — see [`crate::shell`] for the four hermetic layers (the
/// destructive DENYLIST, the DENY-DEFAULT sandbox-exec profile, the consequential
/// park + master/voice-id/lockdown gate routing) and the fifth, device-gated
/// exec seam (built, never invoked in a test).
///
///   - `enabled` (SHIPS ON, full-power default): the master switch. Even ON the
///     tool NEVER auto-runs (see HONESTY below). INERT WITHOUT DEVICE SUPPORT: the
///     exec needs `/usr/bin/sandbox-exec` + `/bin/sh` on-device.
///
/// HONESTY: even with `enabled` true the tool NEVER auto-runs. Every command is
/// CONSEQUENTIAL (it is in `confirm::CONSEQUENTIAL_TOOLS` AND
/// `policy::NEVER_AUTO_APPROVE_TOOLS`, so it parks per-action even under an "Always"
/// policy), so it parks for a spoken human "yes" and only ever executes under the
/// `[integrations].allow_consequential` master switch + the confirm + the voice-id
/// owner gate + `!is_locked_down()`. A destructive/denylisted command is refused
/// PRE-exec and never even parks. The actual execution is DEVICE-gated (it needs
/// `/usr/bin/sandbox-exec` + `/bin/sh` on-device) and is NOT claimed proven by the
/// hermetic tests. A command's output is NEVER fabricated.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    pub enabled: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default), highest-risk. Even ON it NEVER
            // auto-runs: shell_run is in CONSEQUENTIAL_TOOLS + NEVER_AUTO_APPROVE_TOOLS
            // (always parks per-action even under an Always policy), clears a
            // destructive denylist pre-exec, and execs only under
            // allow_consequential + confirm + voice-id + !lockdown in a deny-default
            // sandbox-exec profile. INERT WITHOUT DEVICE SUPPORT: exec needs
            // /usr/bin/sandbox-exec + /bin/sh on-device.
            enabled: true,
        }
    }
}

/// [realm] — SCRATCH REALMS (realm.rs): a disposable, confined build+test sandbox
/// that VERIFIES a `code_propose_diff` proposal in a throwaway, network-denied copy
/// of the user's codebase BEFORE a human applies it — closing the honesty gap the
/// propose path admits (a proposed diff's compile/test correctness is NOT guaranteed).
///
///   - `enabled` (SHIPS ON, full-power default): master switch. INERT WITHOUT DEPS —
///     a Realm can only run with an allowlisted `[code].roots` repo (the tree it
///     COW-copies) AND `[shell].enabled` (it reuses the sandboxed-exec seam). With
///     any of the three unmet the feature is inert (see [`crate::realm::realm_permitted`]).
///   - `verify_command` (SHIPS EMPTY): the build/test command run INSIDE the realm
///     (e.g. `"cargo test --offline"`). EMPTY => the Realm reports an honest
///     UNVERIFIED (there is nothing to run) rather than faking a pass — the operator
///     must set the command their project builds/tests with. The realm is network-
///     denied, so a build must not need the network (pre-fetched deps / `--offline`).
///
/// HONESTY: the realm is a COPY-ON-WRITE copy under `state/realms/<ts>/` — the daemon
/// READS the user's tree to copy it but NEVER writes the real tree; the proposed diff
/// is applied INTO the realm ONLY. Apply-to-real stays the SEPARATE, human-reviewed
/// `scripts/apply_code_diff.sh` (unchanged). The build/test runs under the DENY-DEFAULT,
/// network-denied `sandbox-exec` profile (write-confined to the realm). A `Passed`
/// verdict means the build/test REALLY ran and exited zero — it is never a claim; when
/// the sandbox/tooling is unavailable or the diff does not apply, the Realm reports an
/// honest UNVERIFIED, NEVER a faked pass. The exec is DEVICE-gated (needs
/// `/usr/bin/sandbox-exec` + `/bin/cp` + git) and is NOT claimed proven by the hermetic
/// tests (which use a mock runner).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RealmConfig {
    pub enabled: bool,
    pub verify_command: String,
    /// Wall-clock cap (seconds) for the realm build/test. Compiling from source in a
    /// fresh network-denied realm is far slower than a quick shell command, so this
    /// is generous — a too-short bound makes every real build time out to UNVERIFIED,
    /// defeating the point of the realm (which is a REAL pass/fail verdict).
    pub timeout_secs: u64,
}

impl Default for RealmConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default) — INERT WITHOUT DEPS: a Realm needs an
            // allowlisted [code].roots repo AND [shell].enabled to do anything.
            enabled: true,
            // SHIPS EMPTY — the operator must name the command their project
            // builds/tests with (network-denied, so `--offline` / pre-fetched deps).
            // Empty => an honest UNVERIFIED verdict, never a faked pass.
            verify_command: String::new(),
            // 5 minutes: enough for a real from-source compile+test, so a genuine
            // verdict is reachable instead of always timing out to UNVERIFIED.
            timeout_secs: 300,
        }
    }
}

/// [ui_automation] — GATED UI AUTOMATION (#44, the CAPSTONE), the SINGLE MOST
/// DANGEROUS capability: actually ACTUATING the macOS UI (a synthetic click /
/// type / key combo). It SHIPS ON (full-power default) and is maximally gated by
/// construction — see [`crate::ui_automation`] for the layers (the PURE
/// single-action planner that can never batch, the consequential park PER ACTION
/// + master/voice-id/lockdown gate routing) and the device-gated actuation seam
///   (built, never invoked in a test, and itself behind an Accessibility-TCC consent
///   check).
///
///   - `enabled` (SHIPS ON, full-power default): the master switch. Even ON the
///     tool NEVER auto-runs (see HONESTY below). INERT WITHOUT TCC: the actuation
///     needs Accessibility consent (runtime, not SBPL-grantable) + a real display.
///
/// HONESTY: even with `enabled` true the tool NEVER auto-runs. EVERY actuation is
/// CONSEQUENTIAL (it is in `confirm::CONSEQUENTIAL_TOOLS` AND
/// `policy::NEVER_AUTO_APPROVE_TOOLS`, so it parks per-action even under "Always"),
/// so it parks PER ACTION for a spoken human "yes" — ONE confirm authorizes EXACTLY
/// ONE actuation; a second re-parks — and only ever fires under the `[integrations]
/// .allow_consequential` master switch + the confirm + the voice-id owner gate +
/// `!is_locked_down()`. It is NEVER batched and NEVER autonomous. The actual
/// CGEvent/AX post is DEVICE-gated (it needs the Accessibility TCC consent —
/// runtime user consent, NOT SBPL-grantable — plus a real display) and is NOT
/// claimed proven by the hermetic tests. An actuation result is NEVER fabricated.
/// The Vision app stays READ-ONLY; this actuate op is a SEPARATE, maximally-gated
/// surface.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiAutomationConfig {
    pub enabled: bool,
    /// OPT-IN (SHIPS OFF, default false): when true the final, already-approved
    /// single actuation is POSTED THROUGH the HUD app (DARWIN.app) over the
    /// `state/ipc/actuate.sock` Unix socket instead of by the daemon's own local
    /// CGEvent/AX post. The HUD holds the Accessibility TCC grant, so macOS shows
    /// the clean "DARWIN would like to control this computer using accessibility"
    /// prompt and attributes the grant to the user-facing app. Default FALSE keeps
    /// behavior BYTE-FOR-BYTE unchanged: the existing local CGEvent post runs. This
    /// changes ONLY WHERE the approved action is posted — every gate (the pure
    /// planner, the consequential confirm, the master switch, voice-id, lockdown,
    /// the dry-run preview) runs first, UNCHANGED. In via_app mode the daemon's own
    /// Accessibility check is skipped (the HUD holds the grant); the HUD reports an
    /// honest failure if it is not trusted, which the daemon surfaces faithfully.
    pub actuate_via_app: bool,
}

impl Default for UiAutomationConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default), the single most dangerous capability.
            // Even ON it NEVER auto-runs: ui_actuate is in CONSEQUENTIAL_TOOLS +
            // NEVER_AUTO_APPROVE_TOOLS (parks PER ACTION — one confirm = one
            // actuation; a second re-parks — even under Always), fires only under
            // allow_consequential + confirm + voice-id + !lockdown, never batched.
            // INERT WITHOUT TCC: the CGEvent/AX post needs Accessibility consent
            // (runtime, not SBPL-grantable) + a real display.
            enabled: true,
            // SHIPS OFF: default to the existing local CGEvent post, byte-for-byte
            // unchanged. Operators opt in to route the post through the HUD app so
            // macOS attributes the Accessibility grant to DARWIN.app.
            actuate_via_app: false,
        }
    }
}

/// [vision] — the OPTIONAL on-device VISION-LANGUAGE model (VLM) describe path:
/// the inference `describe_image` op plus the daemon "describe my screen / what
/// am I looking at / describe this image" intent (DISTINCT from the OCR
/// `read.screen` intent). SHIPS ON (full-power default) but INERT WITHOUT A MODEL.
///
///   - `enabled` (SHIPS ON, full-power default): master switch. INERT WITHOUT A
///     MODEL — with an empty `model` the describe intent FALLS BACK honestly (to
///     the OCR `read.screen` path / classification, or an honest "the
///     vision-language model isn't downloaded").
///   - `model` (SHIPS EMPTY): the on-device VLM repo id (a Qwen2-VL-class
///     mlx-vlm model). EMPTY => the server has no VLM to load and the op returns
///     the honest "vlm_unavailable" structure; the daemon NEVER fabricates a
///     description. Set vision.model and download it to engage.
///
/// HONESTY: the VLM runs ON-DEVICE — the image's pixels go ONLY to the local
/// mlx-vlm and NEVER leave the device / never to the cloud. It is DEVICE-GATED:
/// it needs mlx-vlm installed + a multi-GB VLM checkpoint downloaded + enough
/// RAM (slow/absent on smaller chips), so it stays inert until a model is set +
/// downloaded and the op honestly reports when the model isn't available. It is
/// DISTINCT from OCR (OCR =
/// reading text glyphs off the screen; VLM = reasoning about the visual scene).
/// The op + wiring + fallback are tested; the actual description QUALITY is
/// device/runtime-gated and is NEVER claimed measured. No "it can see and
/// understand anything" overclaim.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VisionConfig {
    pub enabled: bool,
    pub model: String,
}

impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default) — INERT WITHOUT A MODEL: vision.model
            // ships empty => the describe op returns honest vlm_unavailable, never
            // fabricates a description. Needs mlx-vlm + a multi-GB Qwen2-VL-class
            // checkpoint downloaded + RAM. Set vision.model and download it to engage.
            enabled: true,
            // SHIPS EMPTY — no VLM is loaded until the operator names one (and
            // downloads it). Empty => the op honestly reports unavailable.
            model: String::new(),
        }
    }
}

/// [image] — the OPTIONAL on-device TEXT->IMAGE generation path (task #18): the
/// inference `generate_image` op (MLX diffusion) plus the daemon "generate /
/// make / draw an image of X" intent. SHIPS ON (full-power default) but INERT
/// WITHOUT A MODEL.
///
///   - `enabled` (SHIPS ON, full-power default): master switch. INERT WITHOUT A
///     MODEL — with an empty `model` the generate-image intent surfaces an honest
///     "the on-device image model isn't set up" line.
///   - `model` (SHIPS EMPTY): the on-device diffusion model id (a FLUX.1-schnell-
///     class mflux checkpoint). EMPTY => the server has no image model to load and
///     the op returns the honest "image_model_unavailable" structure; the daemon
///     NEVER fabricates an image. Set image.model and download it to engage.
///
/// HONESTY: image generation runs 100% ON-DEVICE (MLX diffusion) — the prompt
/// and the generated pixels go ONLY to the local model and the image is saved
/// on-device under state/images/; NOTHING is sent to the cloud (there is NO cloud
/// image API anywhere on this path). It is DEVICE-GATED: it needs an MLX diffusion
/// package installed + a multi-GB checkpoint downloaded + enough RAM (slow/absent
/// on smaller chips), so it stays inert until a model is set + downloaded and the
/// op honestly reports when the model isn't available. The op + wiring + fallback
/// are tested; the actual image QUALITY/speed are device/runtime-gated and are
/// NEVER claimed measured.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ImageConfig {
    pub enabled: bool,
    pub model: String,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default) — INERT WITHOUT A MODEL: image.model
            // ships empty => the op returns honest image_model_unavailable, never
            // fabricates an image. Needs an MLX diffusion pkg + a multi-GB
            // FLUX.1-schnell-class checkpoint + RAM; 100% on-device. Set image.model
            // and download it to engage.
            enabled: true,
            // SHIPS EMPTY — no diffusion model is loaded until the operator names
            // one (and downloads it). Empty => the op honestly reports unavailable.
            model: String::new(),
        }
    }
}

/// [screen_context] — CONTINUOUS SCREEN CONTEXT (#42, screen_context.rs): the
/// MOST privacy-sensitive READ feature. A bounded, redacted, transient in-RAM
/// ring of recent on-screen OCR snapshots, fed by a DEVICE-gated continuous
/// capture loop, recallable by a read-only "what was I working on" intent and
/// wipeable by "forget my screen context". SHIPS ON (full-power default) but INERT
/// WITHOUT TCC, with EXTRA privacy rails because the loop runs CONTINUOUSLY:
///
///   - `enabled` (SHIPS ON, full-power default): the master switch for the
///     CONTINUOUS loop. INERT WITHOUT TCC — it STILL requires runtime macOS
///     Screen-Recording consent; the flag cannot grant the device permission, so on
///     without consent captures NOTHING (the ring never grows, no
///     `screen_context.watching` indicator fires).
///   - `interval_secs` (DEFAULT 30): the cadence at which the device-gated loop
///     grabs ONE frame. Floored to >= 1 (a 0/negative would be a busy loop).
///   - `cap` (DEFAULT 50): the HARD bound on the in-RAM ring — past it the OLDEST
///     entry is evicted (no unbounded accumulation, no disk-spill). Floored to >= 1.
///
/// PRIVACY (every rail enforced, none weakenable here): ships ON by default but
/// INERT WITHOUT Screen-Recording TCC consent; the live loop is TCC-device-gated
/// (the flag cannot grant it, so without consent it captures NOTHING); recognized
/// text is REDACTED before it enters the ring
/// (the optimizer redactor, so an on-screen secret never survives) and is TRANSIENT
/// (in-RAM only — NEVER written to lifelong memory / optimizer traces / disk);
/// the ring is BOUNDED (evict-oldest at `cap`); FORGETTABLE ("forget my screen
/// context" wipes it); a PROMINENT HUD WATCHING indicator fires whenever the loop
/// is active; glyph/text ONLY (never a face/person id/embedding); the pixels NEVER
/// leave the device; READ-ONLY (recall describes, never actuates) and recall NEVER
/// fabricates context (an empty ring is an honest "no recent screen context").
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ScreenContextConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    pub cap: usize,
}

impl Default for ScreenContextConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default), the MOST privacy-sensitive read — INERT
            // WITHOUT TCC: the continuous loop STILL requires runtime macOS
            // Screen-Recording consent; the flag cannot grant it, so without consent
            // it captures nothing. The ring stays redacted + transient (in-RAM, off
            // disk/memory/optimizer) + bounded (cap=50) + forgettable, with the
            // WATCHING indicator.
            enabled: true,
            // A calm cadence — one frame every 30s when on. Floored to >= 1 at use.
            interval_secs: 30,
            // A hard bound on the in-RAM ring; evict-oldest past it. Floored to >= 1.
            cap: 50,
        }
    }
}

impl ScreenContextConfig {
    /// The effective ring cap (>= 1) — a misconfigured 0 would make the ring
    /// useless, so it is floored, never trusted raw.
    pub fn effective_cap(&self) -> usize {
        self.cap.max(1)
    }

    /// The effective capture interval in seconds (>= 1) — a 0/negative would be a
    /// busy loop, so it is floored.
    pub fn effective_interval_secs(&self) -> u64 {
        self.interval_secs.max(1)
    }
}

/// [lumen] — LUMEN: the accessibility SCREEN NARRATOR + hands-free VOICE
/// NAVIGATION (lumen.rs). It NARRATES the focused element / on-screen controls
/// through the speech path (READ-ONLY) and pairs the READ-ONLY OCR/AX locate with
/// the EXISTING per-action-gated `ui_actuate` CAPSTONE (#44) to run ONE voice-named
/// UI action at a time. Lumen only SELECTS the one target + builds the actuation
/// request; the UNCHANGED capstone owns every actuation gate (the pure planner,
/// the consequential spoken confirm PER ACTION, the master switch, voice-id,
/// `!lockdown`). Lumen weakens none of it.
///
///   - `narrate` (SHIPS OFF, default false): CONTINUOUS focus-change narration is
///     EXPLICIT opt-in. OFF => the focus-change path is a strict NO-OP (Lumen
///     speaks nothing on its own); the explicit "read me the screen" request path
///     is unaffected. Continuous narration reads on-screen text aloud, so it is
///     off until the user asks for it.
///   - `max_controls` (DEFAULT 20): the HARD bound on how many on-screen controls
///     one readout narrates / offers for selection — a huge screen is never read
///     wholesale. Floored to >= 1.
///
/// PRIVACY / HONESTY: narration is READ-ONLY and NEVER fabricates an element (an
/// empty focus/screen is spoken honestly); selection NEVER fabricates a target (a
/// miss / an ambiguity REFUSES — never a wrong click); the actuation is DEVICE-
/// gated behind the UNCHANGED capstone (Accessibility TCC + a real display) and
/// the locate is the Vision app's TCC-gated `read.screen` (Screen-Recording); the
/// telemetry frame is SECRET-FREE (role + counts, never the raw label).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LumenConfig {
    pub narrate: bool,
    pub max_controls: usize,
}

impl Default for LumenConfig {
    fn default() -> Self {
        Self {
            // SHIPS OFF — continuous narration reads on-screen text aloud, so it is
            // EXPLICIT opt-in. Off is a strict no-op; the on-request read path still
            // answers.
            narrate: false,
            // A hard bound on how many controls one readout narrates/offers, so a
            // dense screen is never read wholesale. Floored to >= 1 at use.
            max_controls: 20,
        }
    }
}

impl LumenConfig {
    /// The effective control bound (>= 1) — a misconfigured 0 would read nothing,
    /// so it is floored, never trusted raw.
    pub fn effective_max_controls(&self) -> usize {
        self.max_controls.max(1)
    }
}

/// [answers] — answer annotations (anthropic.rs `answers` module): the
/// always-cite source-tracking (#5) and the self-reported confidence (#8). An
/// ADDED honesty layer over the answer, never a change to any safety gate.
///
///   - `cite` (SHIPS ON, full-power default): a turn's answer is followed by a
///     "Sources:" line naming the REAL tool-result sources that actually fed it
///     (the citation-carrying reads — docsearch/unified/recall/episodic/web/
///     integration reads). When the turn used NO retrieval the answer is honestly
///     labeled "from my own knowledge" — NEVER a fabricated citation.
///   - `confidence` (SHIPS ON, full-power default): a bounded instruction asks the
///     model to end its answer with a self-reported confidence (grounded /
///     inferred / uncertain) + a one-line why; the daemon parses + surfaces it.
///   - `verify` (SHIPS ON, full-power default): the self-verification pass (#7). An
///     IMPORTANT turn (a factual / retrieval / consequential turn — the trivial
///     greeting/ack is skipped by the gating heuristic) gets ONE extra self-
///     critique of the DRAFT answer AGAINST the real sources the turn actually
///     used, and AT MOST one bounded revise/annotate when the critique flags an
///     unsupported claim. A second self-check REDUCES hallucination on important
///     turns; it is NOT a correctness guarantee, and it costs one extra model call
///     (needs the cloud tier for the cloud path) — so it is gated AND bounded, and
///     inert on turns the heuristic skips.
///
/// HONESTY: a citation maps to a REAL source that fed the turn (recorded by the
/// per-turn source accumulator from actual tool results), never invented; a
/// no-retrieval turn says "from my own knowledge". Confidence is the model's
/// SELF-REPORT under a gated prompt — the PLUMBING is what the daemon's tests
/// cover; the calibration QUALITY is runtime/model-behavior-gated and is never
/// claimed measured. The verify pass's critique QUALITY is likewise the model's
/// behavior (runtime/model-behavior-gated, never measured) — only the gating +
/// the bounded critique/revise PLUMBING is tested. All SHIP ON (full-power
/// default).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AnswersConfig {
    pub cite: bool,
    pub confidence: bool,
    pub verify: bool,
    /// #21 TOOL-RESULT VERIFICATION. The deterministic plausibility cross-check of
    /// a tool result before it is surfaced as fact / built into a consequential
    /// action.
    pub cross_check: bool,
    /// #21 OPTIONAL bounded model pass sub-flag — the single "does this result look
    /// right for this query?" model call, gated UNDER `cross_check` and OFF by
    /// default (it is a cost). The deterministic layer runs whenever `cross_check`
    /// is on; this only adds the model pass for important results.
    pub cross_check_model_pass: bool,
    /// #22 MULTI-MODEL DEBATE. The conservative, high-stakes-only two-brain debate
    /// + reconcile.
    pub debate: bool,
}

impl Default for AnswersConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default). #5 always-cite: appends a "Sources:" line
            // naming REAL tool-result sources (or "from my own knowledge" when no
            // retrieval ran) — never a fabricated citation. Honesty feature.
            cite: true,
            // SHIPS ON (full-power default). #8 self-reported confidence
            // (grounded/inferred/uncertain + a one-line why). Plumbing is tested;
            // calibration quality is model-behavior-gated (not measured).
            confidence: true,
            // SHIPS ON (full-power default). #7 self-verification: one bounded
            // self-critique of the draft on IMPORTANT turns + at most one revise
            // (skips trivial turns). Costs one extra model call (needs the cloud tier
            // for the cloud path). Reduces hallucination; not a correctness guarantee.
            verify: true,
            // SHIPS ON (full-power default). #21 deterministic tool-result
            // plausibility cross-check before a result is surfaced/built into an
            // action; a tripped check only DOWNGRADES confidence + flags a caveat — it
            // NEVER removes or relaxes a confirmation gate.
            cross_check: true,
            // SHIPS ON (full-power default). #21 optional single model "does this look
            // right?" pass under cross_check (a latency/cost add). Needs the cloud tier
            // for the cloud path; the deterministic layer runs regardless.
            cross_check_model_pass: true,
            // SHIPS ON (full-power default). #22 multi-model debate on high-stakes
            // turns only (conservative predicate; ordinary turns never debate);
            // agreement raises confidence, disagreement surfaces BOTH answers (never a
            // fake consensus); bounded to <=2 model calls. Needs the cloud tier; inert
            // on ordinary turns.
            debate: true,
        }
    }
}

/// [voice_id] — on-device speaker verification (voiceid.rs). An ADDED safety
/// layer, never a replacement for the [integrations] allow_consequential master
/// switch (now ON by default, but still requiring a fresh per-action confirm) or
/// the cross-turn confirmation gate.
///
///   - `enabled` (SHIPS OFF, false): master switch. With it false, OR with no
///     enrolled owner profile, behavior is UNCHANGED from today — `owner_verified`
///     is not enforced anywhere. Turn on deliberately, after explicitly enrolling.
///   - `threshold` (cosine accept on the acoustic embedding): the operating
///     point. Voice/device-dependent — NOT a measured FAR/FRR; tune on the real
///     mic. Higher = stricter (fewer false accepts, more false rejects).
///   - `min_enroll_samples`: how many owner utterances the explicit "enroll my
///     voice" flow captures before a profile is saved.
///   - `gate_scope` ("consequential" default | "all"): "consequential" gates only
///     outward/consequential actions + the confirmation replay (an unrecognized
///     speaker can't act outwardly nor approve a parked action); "all"
///     additionally blocks non-consequential commands. Unknown values fall back to
///     "consequential" (the safe default — never silently to "all").
///
/// HONESTY: this is a LIGHTWEIGHT acoustic model (filterbank statistics +
/// cosine), NOT a high-assurance biometric. It rejects an obviously different
/// voice but is spoofable by replay/impersonation. It FAILS CLOSED for
/// consequential actions (embed error / no usable audio while enabled+enrolled =>
/// treated as unverified, the consequential path is denied) but never bricks an
/// ordinary reply. Raw audio is never persisted; the profile is a local feature
/// vector only.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VoiceIdConfig {
    pub enabled: bool,
    pub threshold: f64,
    pub min_enroll_samples: usize,
    pub gate_scope: String,
}

impl Default for VoiceIdConfig {
    fn default() -> Self {
        Self {
            // SHIPS OFF — voice-id is a fail-closed GATE, not a feature flipped on by
            // the full-power default; enrollment is always explicit.
            enabled: false,
            // The shipped acoustic-embedding default; device-tuned in practice.
            threshold: 0.86,
            min_enroll_samples: 3,
            gate_scope: "consequential".to_string(),
        }
    }
}

/// [threshold] — VOICE-SCOPED GUEST MODE (threshold.rs). A restrict-only GUEST
/// scope applied when voice-id reports an UNRECOGNIZED speaker (or the owner
/// toggles guest mode): a strictly READ-ONLY tool allowlist, recall confined to
/// the SHARED tier (never the owner's private `agent.*` facts, reusing the existing
/// namespace-isolation guard), and a quieter focus profile.
///
/// The guest scope can ONLY narrow the owner scope (restrict-only, proven in
/// threshold.rs). It LAYERS ON TOP of — never replaces — the master switch +
/// per-action confirm + voice-id + policy gates, which are UNCHANGED whether or not
/// guest mode is on. HONESTY: voice-id is a bar-raiser, not a high-assurance
/// biometric (replay-spoofable), so guest mode is a COURTESY boundary, not a
/// security backstop.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThresholdConfig {
    /// Master switch, SHIPS ON (armed by default): an unrecognized speaker is
    /// auto-scoped to guest. ARMED-but-INERT — the "unrecognized" signal only
    /// exists when voice-id is ENFORCING (enrolled), so with voice-id off (the
    /// shipped default) this scopes nothing until a voice is enrolled OR guest mode
    /// is explicitly toggled. False => guest scope never applies (owner behavior
    /// byte-for-byte).
    pub enabled: bool,
    /// The (quiet) focus profile a guest turn uses. Ships "deep_focus" (only a
    /// genuinely CRITICAL signal surfaces). Parsed via
    /// `focus::FocusProfile::from_config_str`, which is restrict-only for ANY string
    /// — so even a typo can only ever quiet, never broaden.
    pub guest_profile: String,
}

impl Default for ThresholdConfig {
    fn default() -> Self {
        Self {
            // SHIPS ARMED (full-power default), but INERT until voice-id is enrolled
            // — see the doc above.
            enabled: true,
            // A genuinely quiet guest lens: only Critical surfaces, no digest.
            guest_profile: "deep_focus".to_string(),
        }
    }
}

/// [forge] — Self-Forge (forge.rs): DARWIN authoring a NEW sandboxed micro-app
/// from a goal. The SAME gated-codegen contract as [self_heal], generalized
/// from "patch the daemon" to "author an app":
///
///   - `enabled` (ships false): master gate. With it false the forge does
///     NOTHING — no cloud draft, no staging, no proposal — exactly like
///     self_heal/allow_consequential.
///   - `mode` ("propose" default; "auto" requires enabled = true): controls
///     what happens to the forge's OWN staged artifact. CRUCIAL DIFFERENCE
///     from self_heal: there is NO auto-DEPLOY path. Even in "auto" the forge
///     may at most do for its staged app what heal's auto does for its staged
///     patch; DEPLOYING a forged app into apps/ (where AppRegistry::discover
///     would pick it up and run it) is ALWAYS a separate human step — the
///     operator runs scripts/apply_forge.sh <ts> after reviewing. No code path
///     in the daemon ever moves a proposal into apps/. Unknown values fall back
///     to "propose" (the safe behavior).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ForgeConfig {
    pub enabled: bool,
    pub mode: String,
}

impl Default for ForgeConfig {
    fn default() -> Self {
        Self {
            // SHIPS ON (full-power default) — INERT WITHOUT A CLOUD KEY: forge_draft
            // requires the cloud key (forge.blocked{reason:"no_api_key"} otherwise).
            enabled: true,
            // KEEP "propose" — propose -> human runs scripts/apply_forge.sh. There is
            // NO auto-deploy path even in mode=auto; deploying into apps/ is always the
            // human apply_forge.sh step. NEVER ship "auto" as the default.
            mode: "propose".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    pub port: u16,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self { port: 7177 }
    }
}

/// [proactive] — three distinct proactivity features share this section:
///   1. The first-contact brief (proactive.rs): when the user returns after
///      more than `idle_gap_hours` away, the next converse reply carries a
///      verified data brief for the persona to phrase. Gated by `enabled`.
///   2. EDITH's anticipation engine (anticipate.rs): the daemon surfaces what
///      matters UNPROMPTED. `speak` is its master switch for SPOKEN output and
///      SHIPS ON (full-power default) — EDITH ALSO voices its brief through the
///      existing echo-safe speech path (is_speaking/MUTE_TAIL/barge cover it, never
///      while already speaking), in addition to the HUD proactive card.
///      `lead_minutes`/`unread_floor`/`quiet_start`/`quiet_end` tune the
///      relevance thresholds and quiet-hours band; the remaining guard knobs
///      (cooldown, rate limit) keep their conservative code defaults.
///   3. The proactive-intelligence suggester (proactive_intel.rs): the habit
///      detector (#13) + predictive suggester (#14). `suggest` is its OWN master
///      switch and SHIPS ON (full-power default), its own gate independent of
///      `enabled`. With `suggest` on the anticipation tick surfaces observed-pattern
///      suggestion cards. The suggester is OBSERVED-pattern-based + propose-only:
///      even on it only SURFACES suggestions — accepting a habit offer still routes
///      through the gated `standing_create` confirmation; DARWIN never auto-acts.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProactiveConfig {
    pub enabled: bool,
    pub idle_gap_hours: u64,
    /// EDITH spoken-proactivity master switch. SHIPS ON (full-power default): voices
    /// the brief through the echo-safe speech path, plus the HUD card.
    pub speak: bool,
    /// Proactive-intelligence suggester master switch (habit detector #13 +
    /// predictive suggester #14, proactive_intel.rs). SHIPS ON (full-power default),
    /// its OWN gate independent of `enabled` — on, the anticipation tick surfaces
    /// observed-pattern suggestion cards; accepting one still routes through the
    /// gated standing_create confirmation (DARWIN never auto-acts).
    pub suggest: bool,
    /// Surface a calendar event this many minutes away (or nearer).
    pub lead_minutes: i64,
    /// Surface important-unread mail at or above this count.
    pub unread_floor: u32,
    /// Quiet-hours band start (local hour, 0-23). Within [start, end) EDITH
    /// stays fully silent (no card either). Wraps midnight when start > end.
    pub quiet_start: u8,
    /// Quiet-hours band end (local hour, 0-23, exclusive). start == end
    /// disables quiet hours.
    pub quiet_end: u8,
}

impl Default for ProactiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_gap_hours: 4,
            // SHIPS ON (full-power default). EDITH spoken-proactivity master gate: on
            // => EDITH also voices its brief through the existing echo-safe speech path
            // (is_speaking/MUTE_TAIL/barge cover it, never while already speaking).
            // 15-min lead, 3-message unread floor, 22:00-07:00 quiet band.
            speak: true,
            // SHIPS ON (full-power default). Habit-detector(#13) + predictive-
            // suggester(#14) master gate: on => surfaces observed-pattern suggestion
            // cards; accepting a habit offer still routes through the gated
            // standing_create confirmation — DARWIN never auto-acts on a suggestion.
            // Independent of `enabled` (which powers the first-contact brief).
            suggest: true,
            lead_minutes: 15,
            unread_floor: 3,
            quiet_start: 22,
            quiet_end: 7,
        }
    }
}

/// [focus] — FOCUS PROFILES (#24, focus.rs). A focus profile is a
/// PERMISSION-NEUTRAL lens over DARWIN's proactive surfaces: it narrows WHICH
/// non-consequential intel reaches the user (which signal categories surface,
/// brief verbosity, whether suggestions are quieted) and can ONLY make DARWIN
/// quieter — never more permissive. By construction (focus.rs) a profile cannot
/// loosen the master switch / confirm gate / voice-id / lockdown / policy, cannot
/// enable a consequential action, and cannot raise autonomy.
///
/// `profile` ships "default" (the IDENTITY — today's behavior byte-for-byte), so
/// the feature ships NEUTRAL. Valid values: "default" | "work" | "sleep" |
/// "deep_focus" | any other string (a named CUSTOM profile, itself restrict-only).
/// A blank/"default" value is the identity; an UNRECOGNIZED non-blank value is a
/// named CUSTOM profile — which is itself restrict-only (it can only quiet, never
/// broaden), so a typo can never accidentally LOOSEN anything. Parsed by
/// `focus::FocusProfile::from_config_str`.
///
/// `auto` (AUTO-FOCUS, focus.rs `select_profile`) ships OFF: when on, the live
/// anticipation tick reselects the active profile each tick from ON-DEVICE signals
/// (acoustic scene + fused presence + calendar + time) and applies it through the
/// SAME restrict-only `apply_profile` path (composed on top of `profile`), so it
/// can only ever narrow further — never broaden past the configured profile,
/// enable an action, or loosen a gate. Opt-in because it changes what surfaces
/// based on sensed room state.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FocusConfig {
    /// The active focus profile name. Ships "default" (the identity). Parsed by
    /// `focus::FocusProfile::from_config_str`; an unknown value is a named custom
    /// profile (restrict-only) and a blank degrades to "default".
    pub profile: String,
    /// AUTO-FOCUS (focus.rs `select_profile`): when true, the LIVE anticipation
    /// tick RESELECTS the active profile each tick from ON-DEVICE signals (acoustic
    /// scene + fused presence + calendar + time-of-day) instead of holding the
    /// static `profile`. Ships OFF: it changes what surfaces based on SENSED room
    /// state, so it is opt-in like the other sensing sections. Auto is
    /// permission-neutral BY CONSTRUCTION — the auto-selected profile is applied
    /// through the SAME restrict-only `apply_profile` path (composed ON TOP of the
    /// configured `profile`), so it can only ever NARROW further, never broaden past
    /// the configured profile or enable/loosen anything. With it OFF, focus behaves
    /// exactly as today (the configured `profile` is used byte-for-byte).
    pub auto: bool,
}

impl Default for FocusConfig {
    /// Ships NEUTRAL: the "default" profile is the identity, reproducing today's
    /// proactive behavior with no profile active. Auto-Focus ships OFF (sensed-
    /// state selection is opt-in).
    fn default() -> Self {
        FocusConfig {
            profile: "default".to_string(),
            auto: false,
        }
    }
}

/// [precog] — PRECOG // WHAT-IF, the counterfactual command simulator
/// (simulate.rs). `enabled` SHIPS ON (full-power default) and is READ-ONLY by
/// construction: the simulate path holds NO actuator / memory-write / inference
/// handle (SimContext carries only read views), so enabling it can only ever let
/// DARWIN DESCRIBE what a real run would do (and that it would PARK behind the
/// gate) — it can never fire an action, even a benign one. With `enabled` false the
/// "what would you do if I said X" cue falls through to ordinary routing.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PrecogConfig {
    /// Whether the PRECOG "what would you do if I said X" cue is answered by the
    /// simulator. SHIPS ON (read-only observability; it never acts).
    pub enabled: bool,
}

impl Default for PrecogConfig {
    /// Ships ON — armed by default, because PRECOG is READ-ONLY (it describes, it
    /// never acts). Mirrors the always-on posture of the other observability
    /// sections ([audit] / [introspect] / [focus]).
    fn default() -> Self {
        PrecogConfig { enabled: true }
    }
}

/// [apps] — the micro-app runtime substrate (docs/SANDBOX.md). `autostart`
/// lists micro-app names darwind launches at startup; it defaults to EMPTY —
/// nothing is autostarted unless the operator opts in. Names that do not match
/// a registered manifest are skipped with a telemetry warning at startup.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AppsConfig {
    pub autostart: Vec<String>,
}

/// [introspect] — the READ-ONLY micro-app introspection sentinel (introspect.rs).
/// `enabled` SHIPS ON: like `[audit]`, it is pure accountability/observability —
/// it watches darwind's own sandboxed children (SBPL profile-drift + RSS/CPU
/// anomalies) and never acts, so enabling it loosens nothing. With it false the
/// sentinel loop is simply not spawned.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct IntrospectConfig {
    /// Master switch for the sentinel loop. SHIPS ON (read-only observability).
    pub enabled: bool,
    /// Seconds between sentinel ticks (profile-drift + resource sample + caps).
    pub interval_secs: u64,
    /// Seconds to wait after boot before the first tick (let apps settle).
    pub startup_delay_secs: u64,
    /// Sustained CPU% above which an app is flagged (resource anomaly).
    pub cpu_alert_percent: f32,
    /// RSS growth multiple over baseline that counts as a leak/runaway.
    pub rss_growth_ratio: f64,
}

impl Default for IntrospectConfig {
    fn default() -> Self {
        // On by default — it only observes darwind's own children and reports.
        // The tuning defaults match introspect.rs's original constants, so an
        // absent/partial [introspect] section behaves exactly as before.
        Self {
            enabled: true,
            interval_secs: 60,
            startup_delay_secs: 30,
            cpu_alert_percent: 95.0,
            rss_growth_ratio: 3.0,
        }
    }
}

/// [persistence] — the PERSISTENCE SENTINEL (persistence.rs): a READ-ONLY
/// "Autoruns for the Mac" inventory of the host's autostart surfaces + per-binary
/// signing/notarization + Gatekeeper, with a pure baseline diff. It only observes
/// and reports (secret-free counts + anomalies); it never remediates.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PersistenceConfig {
    /// Master switch for the sentinel loop. SHIPS ON (read-only observability).
    pub enabled: bool,
    /// Seconds between sentinel scans (the autostart surface moves slowly).
    pub interval_secs: u64,
    /// Seconds to wait after boot before the first scan (let the box settle).
    pub startup_delay_secs: u64,
    /// Whether to run the per-binary `codesign`/`spctl` ASSESSMENT reads. SHIPS ON.
    pub assess_signing: bool,
    /// Cap on how many binaries are signing-assessed per scan (bounds the work).
    pub max_assess: usize,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        // On by default — it only reads the host's autostart surfaces and reports.
        // The cadence matches the TCC sentinel (a slow scan; the surface moves on
        // the order of installs). Signing assessment is on but bounded.
        Self {
            enabled: true,
            interval_secs: 300,
            startup_delay_secs: 45,
            assess_signing: true,
            max_assess: 64,
        }
    }
}

/// [exposure] — the INBOUND EXPOSURE AUDITOR (exposure.rs): a READ-ONLY
/// "nmap-of-self" that reads THIS machine's own listening socket table (via a
/// fixed-arg `netstat -anv`, sending no packets), classifies each socket
/// loopback-only vs network-exposed, and names the macOS sharing service on each
/// exposed well-known port. It only observes and reports (secret-free counts +
/// exposed detail); it closes nothing. The gated `open_settings_pane` actuator is
/// the only remediation and stays behind the standard per-action confirm gate.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExposureConfig {
    /// Master switch for the auditor loop. SHIPS ON (read-only observability).
    pub enabled: bool,
    /// Seconds between scans (the listening surface moves on the order of app
    /// launches, not seconds).
    pub interval_secs: u64,
    /// Seconds to wait after boot before the first scan (let the box settle).
    pub startup_delay_secs: u64,
}

impl Default for ExposureConfig {
    fn default() -> Self {
        // On by default — it only reads the local socket table and reports. The
        // cadence matches the other defensive sentinels (a slow scan).
        Self { enabled: true, interval_secs: 300, startup_delay_secs: 40 }
    }
}

/// [interception] — the TRAFFIC-INTERCEPTION INTEGRITY CHECK (interception.rs): a
/// READ-ONLY "is anything MITMing me?" read of THIS machine's OWN local config — a
/// system/PAC proxy, non-default `/etc/hosts` entries, non-Apple trusted ROOT CAs,
/// the DNS resolvers, and installed configuration/MDM profiles. It sends no packets
/// and never touches another host. It only observes and reports (plain-speech
/// findings + secret-free counts); it closes/removes nothing. Honest SKIP when a
/// read needs a privilege the no-sudo daemon lacks.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InterceptionConfig {
    /// Master switch for the check loop. SHIPS ON (read-only observability).
    pub enabled: bool,
    /// Seconds between checks (local interception config moves on the order of
    /// installs, not seconds).
    pub interval_secs: u64,
    /// Seconds to wait after boot before the first check (let the box settle).
    pub startup_delay_secs: u64,
}

impl Default for InterceptionConfig {
    fn default() -> Self {
        // On by default — it only reads local config and reports. The cadence
        // matches the other defensive sentinels (a slow scan); the startup delay is
        // a touch later so the sentinels don't all fire at once.
        Self { enabled: true, interval_secs: 300, startup_delay_secs: 50 }
    }
}

/// [integrations] — the shared Chart-2 integration substrate (integrations.rs).
/// `allow_consequential` is THE master gate for outward/side-effecting actions
/// (post a message, create an event). It SHIPS ON (true) — the headline of the
/// full-power default. INERT-SAFE: flipping it ON does NOT bypass anything. Every
/// consequential action STILL requires a fresh per-action confirm + voice-id (if
/// enrolled) + !is_locked_down() + the per-action policy at the runtime
/// chokepoints; with this true a CONFIRMED consequential action runs for real
/// instead of returning a DryRun preview. With it false a consequential action
/// returns a DRY-RUN PREVIEW and performs no side effect even when the call site
/// confirmed.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct IntegrationsConfig {
    pub allow_consequential: bool,
}

impl Default for IntegrationsConfig {
    fn default() -> Self {
        // SHIPS ON (full-power default) — the master gate for outward actions is
        // ARMED. This does NOT weaken any gate: a confirmed consequential action
        // still clears confirm + voice-id + policy + !lockdown at the chokepoints;
        // this only decides whether a CONFIRMED action runs for real vs. returns a
        // DryRun preview.
        Self {
            allow_consequential: true,
        }
    }
}

/// [audit] — the append-only, hash-chained, tamper-EVIDENT audit log (audit.rs)
/// of every consequential decision (proposed / parked / blocked-by-policy /
/// auto-approved / confirmed / denied / executed). UNLIKE the autonomy switches,
/// `enabled` SHIPS ON: the log is READ-ONLY accountability — it never acts, only
/// records the decisions the gate already makes, secret-free (the target is
/// redacted) and bounded (prune-oldest + re-root past `max_entries`). With it
/// false NO entry is written and the chokepoints behave byte-for-byte as today.
///
/// HONESTY: the log is tamper-EVIDENT (a hash chain detects mutate/insert/delete/
/// reorder), not tamper-PROOF (a root attacker who can rewrite the whole on-disk
/// chain forward would still verify) — see audit.rs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AuditConfig {
    /// Master switch for recording. SHIPS ON (read-only accountability loosens
    /// nothing). With it false the audit calls at the chokepoints are skipped.
    pub enabled: bool,
    /// Retention cap: past this many entries the oldest are pruned and the chain
    /// re-rooted (truncation keeps the surviving suffix consistent).
    pub max_entries: usize,
}

impl Default for AuditConfig {
    fn default() -> Self {
        // On by default (it only records), bounded by the audit module's cap.
        Self {
            enabled: true,
            max_entries: crate::audit::MAX_ENTRIES,
        }
    }
}

/// [triage] — FORENSIC TRIAGE SNAPSHOT (triage.rs, agent "aegis"). The one-shot
/// "capture everything" op that FREEZES a READ-ONLY, REDACTED, timestamped
/// evidence bundle under `state/forensics/<ts>/` (process tree + signing, socket
/// table, machine/TCC/persistence baselines, a bounded `log show` excerpt of the
/// security subsystems, recent quarantine events) so the owner can hand a
/// professional real evidence. It STRICTLY READS the machine — no kills, no
/// config/security changes, RESTORE is never automated — and NOTHING is
/// transmitted; it writes ONLY under `state/forensics/`. Its bundle digest is
/// folded into the audit chain + the Keychain external anchor. These knobs only
/// BOUND the capture; they never grant a capability.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TriageConfig {
    /// Whole-bundle byte budget: sections are kept while the running total fits,
    /// then truncated/dropped with an honest note. Bounds disk + the capture cost.
    pub max_bundle_bytes: usize,
    /// How many minutes back the bounded security-subsystem `log show` excerpt
    /// reaches. Kept small so the read stays fast + the excerpt stays legible.
    pub log_window_minutes: u64,
}

impl Default for TriageConfig {
    fn default() -> Self {
        // Generous-but-bounded defaults: an 8 MiB bundle and a 1-hour log window.
        // A 0 for either would make a capture pointless, so the defaults are the
        // floor the module actually reads (a config typo of 0 simply yields an
        // empty/near-empty section rather than a crash — honest, never unbounded).
        Self {
            max_bundle_bytes: 8 * 1024 * 1024,
            log_window_minutes: 60,
        }
    }
}

/// [policy] — the per-action policy store (policy.rs): the controlled, USER-SET
/// loosening/hardening BENEATH the [integrations] master switch. SHIPS EMPTY
/// (no rules => Ask everywhere => behavior is exactly today's). Rules are USER-SET
/// ONLY (Settings / the authenticated-local command channel); there is NO
/// tool/agent/model path that can write one, and the rules live in the user-owned
/// state/policy.json (never in this TOML), so the model can't reach them via a
/// config edit either. `enabled` is the layer master switch (ships ON but inert
/// while empty); with it false the layer is bypassed and every action is Ask.
///
/// INVARIANTS (enforced at the chokepoints, not here): a policy can NEVER grant an
/// action the master switch forbids (Always is inert under master OFF); a `Never`
/// rule HARD-BLOCKS even with master ON + a fresh confirmation; the voice-id +
/// confirmation gates remain backstops.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    /// Master switch for the policy layer. SHIPS ON, but the store ships EMPTY so
    /// the layer is inert (Ask everywhere) until the USER sets a rule. With it
    /// false the layer is bypassed entirely (every action is Ask).
    pub enabled: bool,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        // On by default but inert: an empty store evaluates to Ask everywhere, so
        // the shipped behavior is byte-for-byte today's (ASK/park everywhere).
        Self { enabled: true }
    }
}

/// [security] — AT-REST ENCRYPTION of the sensitive local stores (crypto.rs). It
/// CHANGES THE ON-DISK FORMAT (an irreversible migration), so it SHIPS OFF as a
/// deliberate operator opt-in — NOT part of the full-power feature defaults (lose the
/// Keychain master key and the DBs are unrecoverable).
///
///   - `encrypt_memory` (SHIPS OFF, false; PINNED): the master switch. With it
///     false EVERY sensitive store opens via its plaintext `open(path)` with NO
///     `PRAGMA key` — byte-for-byte today's plaintext SQLite (no behavior change,
///     no key, no migration). When the operator flips it true: a fresh 256-bit
///     master key is generated, written to the macOS Keychain (account
///     `memory_encryption_key`), the existing plaintext stores are re-keyed to
///     transparent whole-file SQLCipher AES-256 (a read-plaintext -> write-
///     encrypted migration), and every subsequent open uses `open_encrypted`.
///
/// SCOPE (be honest): ENCRYPTED = the four sensitive SQLite stores (the main Db in
/// memory.rs, docsearch.db, audit.db, the optimize trace store) + the voiceid owner
/// profile (wrapped in its own encrypted SQLCipher blob). NOT ENCRYPTED = the
/// config TOML, the Keychain item itself (already OS-protected), and — critically —
/// the IN-RAM working set + decrypted pages + the key WHILE THE DAEMON RUNS.
/// SQLCipher protects AT REST ON DISK only; it does NOT defend against a live-
/// process/root attacker. Lose the Keychain item => the DBs are unrecoverable.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct SecurityConfig {
    /// Master switch for at-rest encryption. SHIPS OFF (false) and is pinned:
    /// with it false the stores are exactly today's plaintext SQLite.
    pub encrypt_memory: bool,
}

/// [enclave] — ENCLAVE CUSTODY (enclave.rs): ADDITIVE, hardware-bound custody of
/// the at-rest DB master key OVER the existing macOS Keychain path (crypto.rs).
///
///   - `enabled` (SHIPS ON, armed by default): the master switch. When ON AND a
///     Secure Enclave + the SE entitlement are genuinely reachable, the at-rest
///     master key is wrapped by a non-exportable, hardware-bound Secure-Enclave key
///     (`kSecAttrTokenIDSecureEnclave`) — the wrapping key never leaves the chip and
///     cannot be exfiltrated even by root. When OFF, or when the SE is not reachable
///     (the shipped unentitled posture), custody FALLS BACK to the unchanged
///     OS-protected Keychain path.
///
/// ARMED-BUT-INERT (be honest): this ships ON but does nothing until its hardware +
/// entitlement dependency is present — reported as a self-check SKIP + an
/// `enclave.status` frame with `active=false`, NEVER a fabricated "enclave-protected"
/// claim. It is custody-hardening ONLY: it never changes which `SecretKey` startup
/// resolves/installs, and never touches per-agent credential isolation (the
/// integrations allowlist).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EnclaveConfig {
    /// Master switch for Secure-Enclave-bound custody. SHIPS ON (armed) but inert
    /// without the SE hardware + entitlement; with it false custody is exactly
    /// today's Keychain path.
    pub enabled: bool,
}

impl Default for EnclaveConfig {
    fn default() -> Self {
        // Armed by default; inert (honest Keychain fallback) without the Secure
        // Enclave + entitlement dependency.
        Self { enabled: true }
    }
}


/// [distill] — self-distillation (F17). SHIPS OFF: training produces weights (an
/// adapter), a consequential + device-heavy op, so it is a deliberate operator
/// opt-in, NOT a full-power feature default. Even ON, it never auto-promotes an
/// adapter into the live model, and the training run is inert without Apple
/// Silicon + mlx-lm.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DistillConfig {
    /// Master switch. SHIPS OFF (false): with it off the pipeline is a no-op and
    /// the status honestly reports "off".
    pub enabled: bool,
    /// The Python interpreter that runs `mlx_lm.lora` on-device.
    pub python: String,
    /// The base checkpoint the personal adapter attaches to (defaults to the
    /// configured local LLM).
    pub base_model: String,
    /// Bounded training-step count for a run.
    pub iters: u32,
}

impl Default for DistillConfig {
    fn default() -> Self {
        // OFF by default — training mutates weights; a deliberate opt-in.
        Self {
            enabled: false,
            python: "python3".to_string(),
            base_model: "mlx-community/Qwen3-4B-Instruct-2507-4bit".to_string(),
            iters: 200,
        }
    }
}

/// [sync] — federated memory sync (F18). SHIPS OFF: sync moves the user's data
/// off one device (a consequential act), a deliberate opt-in. The shared E2E key
/// lives ONLY in the Keychain (account `sync_shared_key`), never here; the peer
/// endpoint is a non-secret address of the user's OWN device.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct SyncConfig {
    /// Master switch. SHIPS OFF (false): with it off the pipeline is a no-op and
    /// the status honestly reports "off".
    pub enabled: bool,
    /// The user's OWN paired-device endpoint (a non-secret URL). Empty until
    /// paired, which keeps the transport inert.
    pub peer_endpoint: String,
}


/// [scene] — ACOUSTIC SCENE AWARENESS (F6, scene.rs). Classify the ambient
/// soundscape into named sound events. `enabled` is a privacy master switch:
/// continuous ambient classification is opt-in, so it SHIPS OFF.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SceneConfig {
    /// Master switch. SHIPS OFF (false): with it off no classification runs and
    /// the status honestly reports "off". Even ON, the pipeline stays inert
    /// without a bundled classifier model (reported as needs-dependency).
    pub enabled: bool,
    /// Minimum classifier confidence for a detection to surface as an event
    /// (0.0–1.0). Detections below the floor are dropped, never shown.
    pub confidence_floor: f64,
}

impl Default for SceneConfig {
    fn default() -> Self {
        // OFF by default — continuous ambient listening is a privacy-consequential
        // act; opt-in only. A conservative floor keeps low-confidence noise out.
        Self { enabled: false, confidence_floor: 0.6 }
    }
}

/// [overnight] — OVERNIGHT ASYNC AGENTS (F10, overnight.rs). Run queued tasks
/// while the user is away. `enabled` ships OFF (autonomous unattended work is
/// opt-in). `min_gap_secs` throttles the away-gate to once per window.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OvernightConfig {
    /// Master switch. SHIPS OFF (false): with it off no overnight run happens and
    /// the status honestly reports "off". Even ON, runs are cloud-gated (an
    /// Anthropic key must be present) and fire only while the user is away.
    pub enabled: bool,
    /// Minimum seconds between overnight runs — the away-gate won't refire until
    /// this elapses, so one away-window yields one run.
    pub min_gap_secs: i64,
}

impl Default for OvernightConfig {
    fn default() -> Self {
        // OFF by default — running agents unattended is a deliberate opt-in.
        // 6h gap => at most one run per night.
        Self { enabled: false, min_gap_secs: 6 * 3600 }
    }
}

/// [webhooks] — WEBHOOK TRIGGERS (#35, webhooks.rs): an INBOUND network surface
/// that lets an external system trigger a DARWIN intent. The MOST security-
/// sensitive thing added here, so it ships with the strongest fences:
///
///   - `enabled` (SHIPS ON, full-power default): the subsystem master switch. INERT
///     WITHOUT MAPPINGS + SECRET — even on, an unmapped event is rejected and the
///     HMAC secret must be present in the Keychain, so nothing is accepted until the
///     user adds a mapping + sets the secret.
///   - `bind` (defaults to "127.0.0.1"): the listen address. Loopback-ONLY by
///     default; the listener refuses to bind a non-loopback address (the receiver
///     is for a local relay/tunnel, never a public internet listener).
///   - The HMAC secret is NEVER in this TOML. It resolves from the macOS Keychain
///     at account `webhook_hmac_secret` (the same `resolve_secret` machinery the
///     integrations use), so the shared secret never lands in a config/log/Debug.
///   - `mappings` (SHIPS EMPTY): the EXPLICIT event->intent allowlist. An event
///     not named here is REJECTED (never guessed). A mapping whose intent is
///     consequential PARKS for a spoken confirm — a webhook can never auto-execute.
///   - `port` / `max_body_bytes`: bounds (the listen port; a request body cap).
///
/// HONESTY: the live bind/accept-loop is RUNTIME-GATED (wired behind `enabled`,
/// not exercised in tests). The PURE `handle_webhook` decision — verify HMAC,
/// map via the allowlist, route-or-park — is proven hermetically with synthetic
/// signed requests. The secret/body are never logged.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebhooksConfig {
    /// Subsystem master switch. SHIPS ON (full-power default) — INERT WITHOUT
    /// MAPPINGS + SECRET (an unmapped event is rejected; the HMAC secret is required
    /// from the Keychain). The bind stays loopback.
    pub enabled: bool,
    /// Listen address. SHIPS "127.0.0.1" (loopback). A non-loopback value is
    /// refused at bind time (`crate::webhooks::is_loopback_bind`).
    pub bind: String,
    /// Listen port for the loopback receiver.
    pub port: u16,
    /// Hard cap (bytes) on a received request body — a larger body is rejected
    /// rather than buffered, so an oversized POST can never wedge the receiver.
    pub max_body_bytes: usize,
    /// The EXPLICIT event->intent allowlist. SHIPS EMPTY — an unmapped event is
    /// rejected, never guessed. A mapping to a consequential intent still parks.
    pub mappings: Vec<WebhookMapping>,
}

impl Default for WebhooksConfig {
    fn default() -> Self {
        // SHIPS ON (full-power default) — INERT WITHOUT MAPPINGS + SECRET: mappings
        // ship EMPTY (an unmapped event is rejected, never guessed) and the HMAC
        // secret resolves from the Keychain (webhook_hmac_secret). The bind stays
        // loopback 127.0.0.1 (a non-loopback bind is refused). A mapped consequential
        // intent still PARKS for a spoken confirm. Add mappings + set the Keychain
        // secret to use.
        Self {
            enabled: true,
            bind: "127.0.0.1".to_string(),
            port: 8723,
            max_body_bytes: 64 * 1024,
            mappings: Vec::new(),
        }
    }
}

/// One explicit event->intent allowlist entry. `deny_unknown_fields`: a mistyped
/// key is a parse error so a fat-fingered mapping can never silently widen the
/// surface (mirrors [`McpServerConfig`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct WebhookMapping {
    /// The external event name (the `X-Darwin-Event` header / `event` field) this
    /// entry maps. An inbound event whose name matches no mapping is REJECTED.
    pub event: String,
    /// The DARWIN intent the event routes to. If this intent is consequential
    /// (`crate::confirm::is_consequential_tool`) the routed action PARKS for a
    /// spoken confirm instead of executing — a webhook never auto-executes.
    pub intent: String,
}


/// [plugin_sdk] — PLUGIN SDK (#36, plugin_sdk.rs): formalizes + VALIDATES the
/// micro-app capability-module contract — the optional `[intents]`/`[tools]`
/// block a plugin's `manifest.toml` declares (what intents it answers, what tools
/// it exposes, and the capability scopes it requests). `enabled` SHIPS ON
/// (full-power default): the register-on-launch HANDSHAKE scopes a plugin's
/// declared intents/tools onto the live router. The validator itself is PURE and
/// always callable for inspection (`validate_manifest`) regardless of the flag.
///
/// A plugin can NOT request a capability outside the allowed set (the validator
/// rejects an over-privileged manifest), can NOT escape the SBPL default-deny
/// profile (the existing [`AppManifest`] -> `generate_sbpl` derivation is
/// unchanged), and a consequential tool it exposes still rides the gate — so
/// enabling the handshake is safe.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PluginSdkConfig {
    /// Master switch for the live register-on-launch handshake. SHIPS ON
    /// (full-power default). The validator is pure and always available regardless.
    pub enabled: bool,
}

impl Default for PluginSdkConfig {
    fn default() -> Self {
        // SHIPS ON (full-power default). A plugin still cannot request a capability
        // outside the allowed set (the validator rejects over-privileged manifests),
        // cannot escape the default-deny SBPL profile, and any consequential tool it
        // exposes still rides the gate. The validator is pure and always available.
        Self { enabled: true }
    }
}

/// [standing] — Standing Missions (standing.rs): durable, scheduled, autonomous
/// goals that run on the standing-missions scheduler tick (a dedicated runtime
/// loop, distinct from EDITH's anticipation tick) and reason over the World Model.
/// `enabled` is the subsystem MASTER switch and SHIPS ON (full-power default).
/// Even on, the scheduler ([`crate::standing::due_missions`]) is safe: ESTABLISHING
/// a mission is itself confirmation-gated (standing_create is in
/// CONSEQUENTIAL_TOOLS), every consequential step a RUN takes still parks behind
/// the confirmation gate + the [integrations] master switch, and it is bounded to
/// <=8 active missions under FURY caps — so a standing mission can never
/// auto-send/post/spend.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StandingConfig {
    pub enabled: bool,
    /// TRIPWIRE (condition-trigger) EVALUATION CADENCE, seconds: the minimum interval
    /// between evaluations of condition triggers ([`crate::standing::Schedule::Condition`])
    /// against a fresh signal snapshot on the standing scheduler tick. Default 60s.
    /// The standing tick itself bounds the practical maximum frequency; lowering this
    /// only makes tripwires no LESS responsive than the tick. A condition trigger may
    /// only READ + REASON — evaluating it never actuates anything.
    pub condition_eval_secs: u64,
    /// TRIPWIRE DEBOUNCE / RATE-LIMIT, seconds: the minimum interval between successive
    /// FIRES of the SAME condition trigger after it clears — so a flapping signal can
    /// never spam. Combined with the built-in Schmitt hysteresis dead-band, this keeps
    /// a jittery reading from re-firing. Clamped UP to a 5-minute floor
    /// ([`crate::standing::MIN_CONDITION_DEBOUNCE_SECS`]). Default 3600s (1h).
    pub condition_debounce_secs: u64,
}

impl Default for StandingConfig {
    fn default() -> Self {
        // SHIPS ON (full-power default). Even on: establishing a mission (including
        // ARMING a tripwire) is itself confirmation-gated (standing_create is in
        // CONSEQUENTIAL_TOOLS), every consequential step a run takes still parks
        // behind the confirm gate + allow_consequential, and it is bounded to <=8
        // active missions under FURY caps. A standing mission can never
        // auto-send/post/spend. The tripwire cadence/debounce default to a
        // conservative 60s evaluation / 1h re-fire floor.
        Self {
            enabled: true,
            condition_eval_secs: 60,
            condition_debounce_secs: 3600,
        }
    }
}

/// [drafts] — AUTO-DRAFT (#25, drafts.rs): compose a REVIEWABLE pending draft (an
/// email reply / message / doc) the user reads and then sends THEMSELVES through
/// the existing gated send. `enabled` SHIPS ON (full-power default). A draft is
/// ONLY ever a suggestion: the draft module has NO send path, so enabling proactive
/// drafting can never cause an autonomous send. An actual send is a SEPARATE
/// explicit action that rides the existing gate
/// ([integrations].allow_consequential && a fresh confirm) exactly like a normal
/// send. `retention` bounds the persisted pending-draft store (evict-oldest).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DraftsConfig {
    /// Master switch for PROACTIVE drafting. SHIPS ON (full-power default). A draft
    /// is always a reviewable suggestion — this never enables an autonomous send.
    pub enabled: bool,
    /// Evict-oldest cap on persisted pending drafts (bounded store).
    pub retention: usize,
}

impl Default for DraftsConfig {
    fn default() -> Self {
        // SHIPS ON (full-power default). A draft is ALWAYS a reviewable suggestion —
        // the drafts module has NO send path; an actual send is a separate explicit
        // action riding the existing gate (allow_consequential + fresh confirm).
        // Enabling never enables an autonomous send. Bounded store.
        Self { enabled: true, retention: crate::drafts::DEFAULT_RETENTION }
    }
}

/// [missions] — DURABLE MISSIONS (#26, durable_missions.rs): persist FURY mission
/// state (a mission record + per-sub-task status) so a long campaign survives a
/// restart and can be resumed / listed / cancelled. `durable` SHIPS ON (full-power
/// default).
///
/// KEY SAFETY (enforced in durable_missions.rs, not here): (a) a persisted mission
/// does NOT auto-run on restart — it loads as PAUSED and the user must explicitly
/// `resume` it (no silent autonomy); (b) a resumed mission re-runs each
/// consequential sub-task step through the SAME gate (the persistence carries NO
/// pre-approval); (c) it inherits FURY's <=6 sub-task / 1-deep bounds. Enabling only
/// adds persistence, never autonomy. `retention` bounds the persisted mission store
/// (evict-oldest).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MissionsConfig {
    /// Master switch for PERSISTING mission state. SHIPS ON (full-power default). A
    /// persisted mission always loads PAUSED and re-gates its steps — this never
    /// enables auto-run.
    pub durable: bool,
    /// Evict-oldest cap on persisted missions (bounded store).
    pub retention: usize,
}

impl Default for MissionsConfig {
    fn default() -> Self {
        // SHIPS ON (full-power default). KEY SAFETY preserved: a persisted mission
        // does NOT auto-run on restart — it loads PAUSED and the user must explicitly
        // resume; a resumed mission re-runs each consequential step through the SAME
        // gate (persistence carries NO pre-approval); inherits FURY's <=6 sub-task /
        // 1-deep bounds. Enabling only adds persistence, never autonomy. Bounded store.
        Self { durable: true, retention: crate::durable_missions::DEFAULT_RETENTION }
    }
}

/// [macros] — MACRO RECORD/REPLAY (#27, macros.rs): record a NAMED sequence of
/// commands (the utterances/intent names ONLY — NEVER secrets, tokens, or resolved
/// credentials) and replay it. `enabled` SHIPS ON (full-power default).
///
/// KEY SAFETY (enforced in macros.rs + the router, not here): replay re-runs EACH
/// recorded command through the NORMAL router path + the gate EACH time — a
/// consequential step in a macro hits the confirmation gate + the master switch
/// FRESH, exactly as if spoken live (NO pre-approval, NO batching past the gate).
/// The store holds only the recorded utterance + classifier intent name; a secret
/// can never be persisted. Enabling only allows record/replay. `max_steps` bounds a
/// single macro; `retention` bounds the macro store (evict-oldest).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MacrosConfig {
    /// Master switch for recording/replaying macros. SHIPS ON (full-power default).
    pub enabled: bool,
    /// Max commands one macro may hold (a bounded sequence).
    pub max_steps: usize,
    /// Evict-oldest cap on stored macros (bounded store).
    pub retention: usize,
}

impl Default for MacrosConfig {
    fn default() -> Self {
        // SHIPS ON (full-power default). KEY SAFETY preserved: replay re-runs EACH
        // recorded command through the normal router + the gate FRESH (a consequential
        // step hits confirm + the master switch each time — no pre-approval, no
        // batching past the gate); the store holds only utterance + intent name,
        // never a secret. Enabling only allows record/replay. Bounded.
        Self {
            enabled: true,
            max_steps: crate::macros::DEFAULT_MAX_STEPS,
            retention: crate::macros::DEFAULT_RETENTION,
        }
    }
}

/// [skills] — the skill library (skills/). SHIPS ON: the in-tree skills are PURE +
/// read-only, so offering them is safe by default. `enabled` only governs whether
/// the `skill_list` / `skill_invoke` meta-tools are surfaced — a CONSEQUENTIAL skill
/// is STILL parked behind the cross-turn confirmation gate + the [integrations]
/// allow_consequential switch when invoked (a confirmed action still needs a fresh
/// confirm + voice-id + !lockdown), so this flag never lets a side-effecting skill
/// fire unconfirmed.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Master switch for the skill library. SHIPS ON (true) — pure skills are
    /// safe to offer. Set false to hide the meta-tools entirely.
    pub enabled: bool,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        // Ships ON: the in-tree library is pure + read-only and safe by default.
        Self { enabled: true }
    }
}

/// [mcp] — Model Context Protocol client (mcp.rs). The most dangerous external
/// surface in DARWIN: an MCP server is a LOCAL PROCESS (or remote endpoint) that
/// offers tools DARWIN agents can call. `enabled` is the subsystem MASTER switch
/// and SHIPS ON (full-power default) — INERT WITHOUT SERVERS: `servers` ships EMPTY
/// and the installer must NOT add any, so even enabled NOTHING connects until the
/// user adds at least one `[[mcp.servers]]` entry.
///
/// Even with `enabled = true`, every CONSEQUENTIAL MCP tool still parks behind
/// the cross-turn confirmation gate + the [integrations] allow_consequential master
/// switch (a confirmed action still needs that gate + a fresh confirm + voice-id +
/// !lockdown), and a per-server `agents` allowlist controls WHICH agents may use
/// WHICH server. Unknown/mutating tools default to CONSEQUENTIAL (fail-safe). The
/// bounds below cap blast radius regardless.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Subsystem master switch. SHIPS ON (full-power default) — INERT WITHOUT
    /// SERVERS: with an empty `servers` list the manager connects to nothing, so no
    /// tool is discovered or callable. Add a [[mcp.servers]] entry to use.
    pub enabled: bool,
    /// Max servers the manager will connect to at once (bound on fan-out).
    pub max_servers: usize,
    /// Max tools the manager will accept from any one server (bound on a server
    /// that floods `tools/list`).
    pub max_tools_per_server: usize,
    /// Per-call wall-clock ceiling, milliseconds. A server that is slow or hangs
    /// is abandoned at this bound — it never wedges the tool loop.
    pub call_timeout_ms: u64,
    /// Output-size cap, bytes, on any single server response. A response larger
    /// than this is rejected rather than buffered/returned.
    pub max_output_bytes: usize,
    /// The configured servers. SHIPS EMPTY — no server is defined by default, so
    /// even flipping `enabled` true connects to nothing until one is added.
    pub servers: Vec<McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        // SHIPS ON (full-power default), the MOST dangerous external surface — INERT
        // WITHOUT SERVERS: `servers` ships EMPTY and the installer must NOT add any,
        // so even enabled nothing connects until the user adds a [[mcp.servers]]
        // entry. Defense-in-depth always on: every consequential MCP tool parks
        // behind confirm + allow_consequential, unknown/mutating tools default
        // consequential, per-server agents allowlist, default-deny seatbelt, Keychain
        // token. The bounds below cap blast radius regardless.
        Self {
            enabled: true,
            max_servers: 8,
            max_tools_per_server: 64,
            call_timeout_ms: 30_000,
            max_output_bytes: 256 * 1024,
            servers: Vec::new(),
        }
    }
}

/// Transport for one MCP server. `stdio` spawns a local subprocess and exchanges
/// newline-delimited JSON-RPC over its stdin/stdout (the primary local
/// transport). `http` speaks MCP Streamable-HTTP/SSE to a remote HTTPS endpoint
/// (TLS-only; not SBPL-sandboxed — it runs elsewhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum McpTransportKind {
    #[default]
    Stdio,
    Http,
}


/// Default classification for a server's tools when the per-tool overrides do
/// not name one. `consequential` (the default-of-the-default) is fail-safe: an
/// undeclared tool is treated as side-effecting and parks behind the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum McpToolClass {
    ReadOnly,
    #[default]
    Consequential,
}


/// One configured MCP server. A server is INERT until `[mcp].enabled` is true AND
/// it is listed here. `deny_unknown_fields`: a mistyped key is a parse error so a
/// fat-fingered classification or allowlist can never silently widen the surface.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct McpServerConfig {
    /// Server id. Must be the strict shape `[a-z0-9_-]+` with no leading/trailing
    /// or consecutive separator — validated at CONNECT time (not on parse): a name
    /// that fails `integrations::is_safe_mcp_server_name` mints no Keychain account,
    /// so `McpManager::connectable_servers` filters it out and it never spawns a
    /// subprocess or resolves a token. Also the Keychain account stem
    /// (`mcp_<name>_token`) and the sandbox profile filename stem.
    pub name: String,
    /// stdio (local subprocess) or http (remote MCP Streamable-HTTP/SSE).
    pub transport: McpTransportKind,
    /// stdio: the absolute interpreter/binary to spawn. Ignored for http.
    pub command: String,
    /// stdio: argv after `command`. Ignored for http.
    pub args: Vec<String>,
    /// http: the endpoint URL. MUST be `https://` (TLS-only is enforced at
    /// connect so a bearer token never rides plaintext). Ignored for stdio.
    pub url: String,
    /// Optional: the server declares an auth token, resolved from the Keychain at
    /// `mcp_<name>_token` (never inline here, never logged). `false` (default) =
    /// no token. The token never appears in config, Debug, argv, or a URL.
    pub uses_token: bool,
    /// The DARWIN agents permitted to use this server's tools. Default: EMPTY —
    /// no agent may use it until explicitly listed (plus the orchestrator, which
    /// the manager always admits). NEVER auto-grants all agents.
    pub agents: Vec<String>,
    /// Default tool classification for this server when a tool is not named in
    /// `read_only_tools`. Defaults to consequential (fail-safe).
    pub default_class: McpToolClass,
    /// Tool names this server's config asserts are READ-ONLY (safe to call
    /// ungated). Everything else on the server takes `default_class`. An unknown
    /// tool not listed here is therefore consequential by default.
    pub read_only_tools: Vec<String>,
    /// stdio sandbox: extra absolute filesystem subpaths the server is granted
    /// READ access to in its default-deny seatbelt profile (beyond the command
    /// itself). Empty = the command's own dir only.
    pub fs_read: Vec<String>,
    /// stdio sandbox: extra absolute filesystem subpaths the server is granted
    /// WRITE access to. Empty = none.
    pub fs_write: Vec<String>,
    /// stdio sandbox: outbound TCP host-names the server may reach. Empty = NO
    /// network at all (default-deny). A network-needing stdio server must
    /// declare its hosts here, honestly narrowing the profile.
    pub net_hosts: Vec<String>,
}


impl Config {
    /// Load the config plus a list of human-readable issues (unknown keys,
    /// invalid sections). Issues are warned here immediately; the caller
    /// re-emits them as config.invalid telemetry once the hub exists —
    /// Config::load runs before telemetry::init, so emitting here would be
    /// silently dropped (audit fix: misconfiguration used to be a buried
    /// log WARN on an appliance whose only live signal is the HUD).
    pub fn load(path: &Path) -> (Config, Vec<String>) {
        match std::fs::read_to_string(path) {
            Ok(raw) => Self::parse(&raw),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No file is a supported state (hardcoded contract defaults),
                // not a misconfiguration.
                warn!(path = %path.display(), "config file missing; using contract defaults");
                (Config::default(), Vec::new())
            }
            Err(e) => {
                let issue = format!("config unreadable ({e}); using contract defaults");
                warn!(path = %path.display(), "{issue}");
                (Config::default(), vec![issue])
            }
        }
    }

    /// Load for a PERIODIC LIVE-RELOAD reader (the audit-snapshot status
    /// emitters): returns the parsed config ONLY when the file genuinely reads
    /// and parses as top-level TOML. `None` on a missing/unreadable/EMPTY file
    /// or a TOML syntax error — exactly what a transient mid-save truncation
    /// looks like — so the caller keeps its LAST-GOOD view instead of emitting
    /// fabricated contract defaults for one tick. Per-key warnings still load
    /// (identical to boot, which proceeds on the same warnings). Deliberately
    /// conservative: an operator who truly empties/deletes the config gets the
    /// defaults at the next daemon restart, not from a live blip.
    pub fn load_live(path: &Path) -> Option<Config> {
        let raw = std::fs::read_to_string(path).ok()?;
        if raw.trim().is_empty() {
            return None; // a 0-byte truncate-then-write window, not a config
        }
        if raw.parse::<toml::Table>().is_err() {
            return None; // mid-save partial write / syntax error -> keep last good
        }
        Some(Self::parse(&raw).0)
    }

    /// Parse with per-section fallback (audit fix): one wrong-typed key used
    /// to silently revert EVERY other customization to hardcoded defaults.
    /// Now a section that fails to deserialize falls back alone, every other
    /// section keeps its configured values, and unknown sections/keys are
    /// reported instead of vanishing.
    fn parse(raw: &str) -> (Config, Vec<String>) {
        let mut issues = Vec::new();
        let table: toml::Table = match raw.parse() {
            Ok(table) => table,
            Err(e) => {
                let issue = format!("config has a TOML syntax error ({e}); using contract defaults");
                warn!("{issue}");
                return (Config::default(), vec![issue]);
            }
        };

        // Unknown-key diagnostics: a typo'd section or key means the operator
        // believes a tuning change is active when it is not.
        for (section, value) in &table {
            match KNOWN_KEYS.iter().find(|(name, _)| name == section) {
                None => {
                    let issue = format!("unknown config section [{section}] ignored");
                    warn!("{issue}");
                    issues.push(issue);
                }
                Some((_, keys)) => {
                    if let Some(entries) = value.as_table() {
                        for key in entries.keys() {
                            if !keys.contains(&key.as_str()) {
                                let issue = format!("unknown config key {section}.{key} ignored");
                                warn!("{issue}");
                                issues.push(issue);
                            }
                        }
                    }
                }
            }
        }

        let cfg = Config {
            audio: section(&table, "audio", &mut issues),
            models: section(&table, "models", &mut issues),
            router: section(&table, "router", &mut issues),
            local_tools: section(&table, "local_tools", &mut issues),
            cloud: section(&table, "cloud", &mut issues),
            speech: section(&table, "speech", &mut issues),
            inference: section(&table, "inference", &mut issues),
            self_heal: section(&table, "self_heal", &mut issues),
            forge: section(&table, "forge", &mut issues),
            telemetry: section(&table, "telemetry", &mut issues),
            proactive: section(&table, "proactive", &mut issues),
            focus: section(&table, "focus", &mut issues),
            apps: section(&table, "apps", &mut issues),
            introspect: section(&table, "introspect", &mut issues),
            persistence: section(&table, "persistence", &mut issues),
            exposure: section(&table, "exposure", &mut issues),
            interception: section(&table, "interception", &mut issues),
            integrations: section(&table, "integrations", &mut issues),
            standing: section(&table, "standing", &mut issues),
            drafts: section(&table, "drafts", &mut issues),
            missions: section(&table, "missions", &mut issues),
            macros: section(&table, "macros", &mut issues),
            mcp: section(&table, "mcp", &mut issues),
            skills: section(&table, "skills", &mut issues),
            optimize: section(&table, "optimize", &mut issues),
            explain: section(&table, "explain", &mut issues),
            calibrate: section(&table, "calibrate", &mut issues),
            mirror: section(&table, "mirror", &mut issues),
            voice_id: section(&table, "voice_id", &mut issues),
            threshold: section(&table, "threshold", &mut issues),
            episodic: section(&table, "episodic", &mut issues),
            notebooks: section(&table, "notebooks", &mut issues),
            lifelog: section(&table, "lifelog", &mut issues),
            voice: section(&table, "voice", &mut issues),
            wake: section(&table, "wake", &mut issues),
            interpret: section(&table, "interpret", &mut issues),
            docsearch: section(&table, "docsearch", &mut issues),
            code: section(&table, "code", &mut issues),
            shell: section(&table, "shell", &mut issues),
            ui_automation: section(&table, "ui_automation", &mut issues),
            vision: section(&table, "vision", &mut issues),
            image: section(&table, "image", &mut issues),
            screen_context: section(&table, "screen_context", &mut issues),
            lumen: section(&table, "lumen", &mut issues),
            answers: section(&table, "answers", &mut issues),
            audit: section(&table, "audit", &mut issues),
            triage: section(&table, "triage", &mut issues),
            policy: section(&table, "policy", &mut issues),
            security: section(&table, "security", &mut issues),
            enclave: section(&table, "enclave", &mut issues),
            distill: section(&table, "distill", &mut issues),
            sync: section(&table, "sync", &mut issues),
            scene: section(&table, "scene", &mut issues),
            overnight: section(&table, "overnight", &mut issues),
            webhooks: section(&table, "webhooks", &mut issues),
            plugin_sdk: section(&table, "plugin_sdk", &mut issues),
            power: section(&table, "power", &mut issues),
            report: section(&table, "report", &mut issues),
            chart: section(&table, "chart", &mut issues),
            artifact: section(&table, "artifact", &mut issues),
            boundary: section(&table, "boundary", &mut issues),
            vault: section(&table, "vault", &mut issues),
            egress: section(&table, "egress", &mut issues),
            precog: section(&table, "precog", &mut issues),
            realm: section(&table, "realm", &mut issues),
        };

        // SELECTABLE QUANTIZATION (#39) value validation: an unknown [inference]
        // .quant (e.g. "int3", "8bit") is a misconfiguration — the operator
        // believes a quant override is active when it is not. Report it AND keep
        // the neutral "auto" default (today's behavior) rather than pass a bogus
        // value to the server. PURE; mirrors server.py's validate_quant reject.
        let mut cfg = cfg;
        if !InferenceConfig::quant_is_valid(&cfg.inference.quant) {
            let issue = format!(
                "inference.quant = {:?} is not one of {:?}; keeping \"auto\"",
                cfg.inference.quant,
                InferenceConfig::ALLOWED_QUANT,
            );
            warn!("{issue}");
            issues.push(issue);
            cfg.inference.quant = "auto".to_string();
        }
        (cfg, issues)
    }
}

/// Deserialize one named section, falling back to that section's defaults —
/// and recording the issue — when it is malformed. Missing sections are the
/// normal defaulted case, not an issue.
fn section<T: DeserializeOwned + Default>(
    table: &toml::Table,
    name: &str,
    issues: &mut Vec<String>,
) -> T {
    match table.get(name) {
        None => T::default(),
        Some(value) => match value.clone().try_into() {
            Ok(parsed) => parsed,
            Err(e) => {
                let issue = format!("config section [{name}] invalid ({e}); using defaults for this section only");
                warn!("{issue}");
                issues.push(issue);
                T::default()
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    /// Audit fix: a single wrong-typed key must only revert ITS section —
    /// the old whole-file fallback silently discarded every other
    /// customization (voice, thresholds, telemetry port).
    #[test]
    fn bad_section_falls_back_alone() {
        let raw = r#"
            [audio]
            rms_threshold = "loud"   # wrong type: this section reverts

            [speech]
            voice = "bf_emma"

            [telemetry]
            port = 7999
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert_eq!(cfg.audio.rms_threshold, 0.015, "bad section -> its defaults");
        assert_eq!(cfg.speech.voice, "bf_emma", "good sections must survive");
        assert_eq!(cfg.telemetry.port, 7999);
        assert!(
            issues.iter().any(|i| i.contains("[audio]")),
            "the failed section must be reported: {issues:?}"
        );
    }

    #[test]
    fn unknown_sections_and_keys_are_reported_not_swallowed() {
        let raw = r#"
            [audio]
            rms_treshold = 0.02      # typo: must be diagnosed, value unused

            [telemtry]
            port = 7177
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert_eq!(cfg.audio.rms_threshold, 0.015, "typo'd key never applies");
        assert!(issues.iter().any(|i| i.contains("audio.rms_treshold")), "{issues:?}");
        assert!(issues.iter().any(|i| i.contains("[telemtry]")), "{issues:?}");
    }

    #[test]
    fn syntax_error_reverts_to_defaults_with_an_issue() {
        let (cfg, issues) = Config::parse("not [valid toml");
        assert_eq!(cfg.telemetry.port, 7177);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("syntax"));
    }

    #[test]
    fn clean_config_parses_with_no_issues() {
        let raw = r#"
            [proactive]
            enabled = true
            idle_gap_hours = 6

            [self_heal]
            enabled = false
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "{issues:?}");
        assert!(cfg.proactive.enabled);
        assert_eq!(cfg.proactive.idle_gap_hours, 6);
    }

    /// MULTI-RESIDENT LOCAL warm-set (task #17): the CONSERVATIVE default is
    /// SINGLE-RESIDENT — an empty warm-set + a 0 budget, exactly today's behavior
    /// and the safe state on a low-RAM Mac. This PINS that default so it cannot
    /// silently flip to multi-resident.
    #[test]
    fn local_warm_set_defaults_are_conservative_single_resident() {
        let cfg = Config::default();
        assert!(cfg.models.local_warm.is_empty(), "default warm-set must be empty");
        assert_eq!(cfg.models.local_budget_gib, 0.0, "default budget must be 0 (single-resident)");
        assert!(cfg.models.local_sizes.is_empty(), "default sizes table must be empty");
    }

    /// The multi-resident keys ARE known (no unknown-key diagnostic) and parse
    /// into ModelsConfig. A configured warm-set + budget round-trips cleanly.
    #[test]
    fn local_warm_set_keys_are_known_and_parse() {
        let raw = r#"
            [models]
            local_warm = ["mlx-community/Qwen3-0.6B-4bit"]
            local_budget_gib = 3.0
            local_sizes = { "mlx-community/Qwen3-0.6B-4bit" = 0.5 }
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(
            !issues.iter().any(|i| i.contains("models.local")),
            "local_* keys must be KNOWN (no unknown-key diagnostic): {issues:?}"
        );
        assert_eq!(cfg.models.local_warm, vec!["mlx-community/Qwen3-0.6B-4bit"]);
        assert_eq!(cfg.models.local_budget_gib, 3.0);
        assert_eq!(
            cfg.models.local_sizes.get("mlx-community/Qwen3-0.6B-4bit"),
            Some(&0.5)
        );
    }

    // --- #37 SPECULATIVE DECODING + #39 QUANTIZATION defaults (OFF/neutral) ----

    /// #37 + #39: speculative SHIPS ON (full-power default) but is INERT WITHOUT a
    /// loadable `draft_model` (ships ""), and `quant` ships "auto" (neutral). This
    /// PINS the new ON default for speculative + the empty draft_model (so it stays
    /// honestly inert until a model is supplied) + the neutral quant default.
    #[test]
    fn inference_speculative_and_quant_default_on_inert_until_model() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty(), "{issues:?}");
        assert!(cfg.inference.preload, "preload stays today's default (true)");
        assert!(
            cfg.inference.speculative,
            "speculative SHIPS ON (full-power default; inert without a draft model)"
        );
        assert!(
            cfg.inference.draft_model.is_empty(),
            "draft_model MUST ship empty (no draft => speculative honestly inert, reports speculative=false)"
        );
        assert_eq!(
            cfg.inference.quant, "auto",
            "quant MUST ship \"auto\" (== today's behavior, load as configured)"
        );
    }

    /// #37 + #39: the new [inference] keys are KNOWN (no unknown-key diagnostic)
    /// and round-trip. A configured draft model + speculative + an allowed quant
    /// parse cleanly.
    #[test]
    fn inference_speculative_and_quant_keys_are_known_and_parse() {
        let raw = r#"
            [inference]
            speculative = true
            draft_model = "mlx-community/Qwen3-0.6B-4bit"
            quant = "int4"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(
            !issues.iter().any(|i| i.contains("inference")),
            "[inference] keys must be KNOWN (no diagnostic): {issues:?}"
        );
        assert!(cfg.inference.speculative);
        assert_eq!(cfg.inference.draft_model, "mlx-community/Qwen3-0.6B-4bit");
        assert_eq!(cfg.inference.quant, "int4");
    }

    /// #39: every allowed quant value validates; an unknown value is REJECTED by
    /// the pure validator (mirrors server.py's validate_quant accept/reject).
    #[test]
    fn quant_validator_accepts_allowed_rejects_unknown() {
        for q in ["auto", "fp16", "int8", "int4"] {
            assert!(super::InferenceConfig::quant_is_valid(q), "{q} must be allowed");
        }
        for q in ["int3", "8bit", "bf16", "INT4", "", "fp32"] {
            assert!(!super::InferenceConfig::quant_is_valid(q), "{q} must be rejected");
        }
    }

    /// #39: an UNKNOWN [inference].quant is reported as a config issue AND kept at
    /// the neutral "auto" default — never passed bogus to the server (honest: the
    /// operator believes a quant override is active when it is not).
    #[test]
    fn unknown_quant_reported_and_falls_back_to_auto() {
        let raw = r#"
            [inference]
            quant = "int3"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(
            issues.iter().any(|i| i.contains("quant") && i.contains("int3")),
            "an unknown quant must be reported: {issues:?}"
        );
        assert_eq!(
            cfg.inference.quant, "auto",
            "an unknown quant must fall back to the neutral default, never pass through"
        );
    }

    /// #38: [power] adaptive throttling SHIPS ON (full-power default; PERF-ONLY —
    /// influences only the local model sub-choice, never a gate/cloud call), with the
    /// conservative low_battery_pct = 20 default. The keys are KNOWN and round-trip.
    #[test]
    fn power_adaptive_defaults_on_and_keys_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty(), "{issues:?}");
        assert!(
            cfg.power.adaptive,
            "[power].adaptive SHIPS ON (full-power default; perf-only, device-gated read)"
        );
        assert_eq!(cfg.power.low_battery_pct, 20);

        let raw = r#"
            [power]
            adaptive = true
            low_battery_pct = 15
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(
            !issues.iter().any(|i| i.contains("power")),
            "[power] keys must be KNOWN (no diagnostic): {issues:?}"
        );
        assert!(cfg.power.adaptive);
        assert_eq!(cfg.power.low_battery_pct, 15);
    }

    /// AUTO-FOCUS: [focus].profile ships "default" (the identity) and [focus].auto
    /// ships OFF (sensed-state selection is opt-in). Both keys are KNOWN (no
    /// unknown-key diagnostic) and round-trip, and enabling auto takes.
    #[test]
    fn focus_defaults_neutral_auto_off_and_keys_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(cfg.focus.profile, "default", "[focus].profile ships the identity");
        assert!(!cfg.focus.auto, "[focus].auto ships OFF (opt-in sensed-state selection)");

        let raw = r#"
            [focus]
            profile = "work"
            auto = true
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(
            !issues.iter().any(|i| i.contains("focus")),
            "[focus] keys must be KNOWN (no diagnostic): {issues:?}"
        );
        assert_eq!(cfg.focus.profile, "work");
        assert!(cfg.focus.auto, "enabling auto must take");
    }

    /// Contract lockstep: [proactive] defaults are enabled=true,
    /// idle_gap_hours=4 — exactly what config/darwin.toml ships.
    #[test]
    fn proactive_defaults_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.proactive.enabled);
        assert_eq!(cfg.proactive.idle_gap_hours, 4);
    }

    /// Contract lockstep: [proactive].speak (EDITH's spoken-proactivity master
    /// switch) SHIPS ON (full-power default) — EDITH also voices its brief through
    /// the echo-safe speech path (plus the HUD card). The key (and the EDITH tuning
    /// keys) must parse without an unknown-key diagnostic, and flipping speak off
    /// must take.
    #[test]
    fn proactive_speak_defaults_on_and_edith_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.proactive.speak,
            "EDITH spoken proactivity SHIPS ON (full-power default; echo-safe speech path)"
        );
        // The conservative tuning defaults.
        assert_eq!(cfg.proactive.lead_minutes, 15);
        assert_eq!(cfg.proactive.unread_floor, 3);
        assert_eq!(cfg.proactive.quiet_start, 22);
        assert_eq!(cfg.proactive.quiet_end, 7);

        let raw = r#"
            [proactive]
            speak = false
            lead_minutes = 30
            unread_floor = 5
            quiet_start = 23
            quiet_end = 6
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "EDITH keys must all be known: {issues:?}");
        assert!(!cfg.proactive.speak, "the operator can turn spoken proactivity off");
        assert_eq!(cfg.proactive.lead_minutes, 30);
        assert_eq!(cfg.proactive.unread_floor, 5);
        assert_eq!(cfg.proactive.quiet_start, 23);
        assert_eq!(cfg.proactive.quiet_end, 6);
    }

    /// Lockstep with the SHIPPED file: config/darwin.toml must parse with
    /// zero diagnostics and carry exactly the contract defaults the structs
    /// fall back to — if either side drifts, this fails.
    #[test]
    fn shipped_config_file_parses_cleanly_and_matches_defaults() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config")
            .join("darwin.toml");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let (cfg, issues) = Config::parse(&raw);
        assert!(issues.is_empty(), "shipped config has diagnostics: {issues:?}");
        let defaults = Config::default();
        assert_eq!(cfg.self_heal.enabled, defaults.self_heal.enabled);
        assert_eq!(cfg.self_heal.mode, defaults.self_heal.mode);
        assert!(cfg.self_heal.enabled, "self-heal SHIPS ON (full-power default; inert without a cloud key)");
        assert_eq!(cfg.self_heal.mode, "propose", "self-heal stays PROPOSE (the gate; never auto)");
        assert_eq!(cfg.forge.enabled, defaults.forge.enabled);
        assert_eq!(cfg.forge.mode, defaults.forge.mode);
        assert!(cfg.forge.enabled, "self-forge SHIPS ON (full-power default; inert without a cloud key)");
        assert_eq!(cfg.forge.mode, "propose", "forge stays PROPOSE (the gate; never auto-deploy)");
        assert_eq!(cfg.answers.cite, defaults.answers.cite);
        assert_eq!(cfg.answers.confidence, defaults.answers.confidence);
        assert_eq!(cfg.answers.verify, defaults.answers.verify);
        assert!(cfg.answers.cite, "answer citations SHIP ON (full-power default)");
        assert!(cfg.answers.confidence, "answer confidence SHIPS ON (full-power default)");
        assert!(cfg.answers.verify, "answer self-verification SHIPS ON (full-power default)");
        assert_eq!(cfg.proactive.enabled, defaults.proactive.enabled);
        assert_eq!(cfg.proactive.idle_gap_hours, defaults.proactive.idle_gap_hours);
        assert_eq!(cfg.proactive.speak, defaults.proactive.speak);
        assert!(cfg.proactive.speak, "EDITH spoken proactivity SHIPS ON (full-power default)");
        assert_eq!(cfg.proactive.suggest, defaults.proactive.suggest);
        assert!(cfg.proactive.suggest, "proactive-intel suggester SHIPS ON (full-power default)");
        assert_eq!(cfg.cloud.heavy_model, defaults.cloud.heavy_model);
        assert_eq!(cfg.telemetry.port, defaults.telemetry.port);
        assert_eq!(cfg.speech.instant_opener, defaults.speech.instant_opener);
        assert!(
            !cfg.speech.instant_opener,
            "the canned instant opener now SHIPS OFF — the persona greets/answers naturally"
        );
        assert_eq!(
            cfg.integrations.allow_consequential,
            defaults.integrations.allow_consequential
        );
        assert!(
            cfg.integrations.allow_consequential,
            "the consequential master gate SHIPS ON (full-power default; ARMED but still per-action gated)"
        );
        assert_eq!(cfg.standing.enabled, defaults.standing.enabled);
        assert!(cfg.standing.enabled, "standing missions SHIP ON (full-power default; every consequential step still parks)");
        // [code] (task #16): code intelligence SHIPS ON but is INERT without an allowlisted root.
        assert_eq!(cfg.code.enabled, defaults.code.enabled);
        assert!(cfg.code.enabled, "code intelligence SHIPS ON (full-power default; inert without a root)");
        assert!(cfg.code.roots.is_empty(), "no codebase root is allowlisted by default (the installer must not guess)");
        assert_eq!(cfg.code.max_diff_bytes, defaults.code.max_diff_bytes);
        assert!(cfg.code.max_diff_bytes > 0, "the proposed-diff size bound is finite");
        assert_eq!(
            cfg.router.cloud_confidence_threshold,
            defaults.router.cloud_confidence_threshold
        );
        assert_eq!(cfg.router.conversation_route, defaults.router.conversation_route);
    }

    /// CONTINUOUS SCREEN CONTEXT (#42): [screen_context] SHIPS ON (full-power
    /// default) but is INERT WITHOUT TCC — the continuous loop still requires runtime
    /// macOS Screen-Recording consent, which the flag cannot grant. Prove the default
    /// + the empty-config parse are ON, the bounds are sane (cap >= 1, interval >= 1),
    ///   and the keys are known (no unknown-key diagnostic).
    #[test]
    fn screen_context_ships_on_inert_without_tcc_with_sane_bounds_and_known_keys() {
        // The Default impl is ON (inert without TCC consent).
        let d = super::ScreenContextConfig::default();
        assert!(d.enabled, "continuous screen context SHIPS ON (inert without Screen-Recording TCC)");
        assert_eq!(d.cap, 50);
        assert_eq!(d.interval_secs, 30);
        assert!(d.effective_cap() >= 1);
        assert!(d.effective_interval_secs() >= 1);

        // An empty config (no [screen_context] block) parses to ON, no diagnostic.
        let (cfg, issues) = Config::parse("");
        assert!(
            cfg.screen_context.enabled,
            "an absent [screen_context] block falls back to the ON default"
        );
        assert!(
            issues.iter().all(|i| !i.contains("screen_context")),
            "the [screen_context] keys must be known (no unknown-key diagnostic): {issues:?}"
        );

        // The keys take + a misconfigured 0 cap/interval is FLOORED, never trusted.
        let (cfg, issues) = Config::parse(
            "[screen_context]\nenabled = true\ninterval_secs = 0\ncap = 0\n",
        );
        assert!(issues.is_empty(), "valid keys parse clean: {issues:?}");
        assert!(cfg.screen_context.enabled);
        assert_eq!(cfg.screen_context.effective_cap(), 1, "a 0 cap is floored to 1");
        assert_eq!(
            cfg.screen_context.effective_interval_secs(),
            1,
            "a 0 interval is floored to 1 (never a busy loop)"
        );

        // A typo'd key under [screen_context] IS flagged (lockstep with KNOWN_KEYS).
        let (_cfg, issues) = Config::parse("[screen_context]\nenable = true\n");
        assert!(
            issues.iter().any(|i| i.contains("enable")),
            "a typo'd [screen_context] key must be diagnosed: {issues:?}"
        );
    }

    /// Contract lockstep: [router].conversation_route ships "cloud_heavy" —
    /// conversation is answered by cloud Opus by default (the local 4B is the
    /// offline fallback). The key must parse without an unknown-key diagnostic,
    /// and the other two allowed values must take.
    #[test]
    fn conversation_route_defaults_cloud_heavy_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert_eq!(
            cfg.router.conversation_route, "cloud_heavy",
            "conversation must default to cloud Opus"
        );

        for value in ["cloud_fast", "local", "cloud_heavy"] {
            let raw = format!("[router]\nconversation_route = \"{value}\"\n");
            let (cfg, issues) = Config::parse(&raw);
            assert!(
                issues.is_empty(),
                "conversation_route must be a known key: {issues:?}"
            );
            assert_eq!(cfg.router.conversation_route, value);
        }
    }

    /// Contract lockstep: [speech].instant_opener SHIPS OFF (owner preference) — the
    /// canned "Right away, sir." task-ack does NOT play by default; the persona
    /// answers naturally from its first word. The key must parse without an
    /// unknown-key diagnostic, and turning it ON must take.
    #[test]
    fn instant_opener_defaults_off_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.speech.instant_opener,
            "the canned opener SHIPS OFF (owner preference)"
        );

        let raw = r#"
            [speech]
            instant_opener = true
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "instant_opener must be a known key: {issues:?}");
        assert!(cfg.speech.instant_opener, "the operator can turn the canned opener back on");
    }

    /// Contract lockstep: [self_heal] ships enabled=TRUE (full-power default; inert
    /// without a cloud key), mode="propose" (the gate — KEPT, never auto) — exactly
    /// what config/darwin.toml carries — and both keys parse without unknown-key
    /// diagnostics.
    #[test]
    fn self_heal_defaults_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.self_heal.enabled, "self-heal SHIPS ON (full-power default; inert without a cloud key)");
        assert_eq!(cfg.self_heal.mode, "propose", "self-heal stays PROPOSE (the gate; never auto)");

        let raw = r#"
            [self_heal]
            enabled = true
            mode = "auto"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "mode must be a known key: {issues:?}");
        assert!(cfg.self_heal.enabled);
        assert_eq!(cfg.self_heal.mode, "auto");
    }

    /// Contract lockstep: [optimize] ships enabled=TRUE (full-power default),
    /// mode="propose" (KEPT — the optimizer only PROPOSES, never auto-applies), the
    /// SAME shape as [self_heal]/[forge] — and both keys parse without unknown-key
    /// diagnostics. Live trace recording is runtime-gated (enforced in optimize.rs);
    /// this only pins the gate + key spelling.
    #[test]
    fn optimize_defaults_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.optimize.enabled, "the optimizer SHIPS ON (full-power default)");
        assert_eq!(cfg.optimize.mode, "propose", "optimizer stays PROPOSE (never auto-apply-to-live)");

        let raw = r#"
            [optimize]
            enabled = true
            mode = "auto"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "enabled+mode must be known keys: {issues:?}");
        assert!(cfg.optimize.enabled);
        assert_eq!(cfg.optimize.mode, "auto");
    }

    /// Contract lockstep: [answers] ships cite=true, confidence=true, verify=true
    /// (full-power default) — the answer-annotation + self-verification features are ON
    /// by default — and all three keys parse without unknown-key diagnostics. They
    /// reduce hallucination / add honest annotations (enforced in anthropic.rs); this
    /// pins the new ON default + key spelling. A typo is diagnosed, not silently
    /// swallowed.
    #[test]
    fn answers_defaults_on_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.answers.cite, "answer citations SHIP ON (full-power default)");
        assert!(cfg.answers.confidence, "answer confidence SHIPS ON (full-power default)");
        assert!(cfg.answers.verify, "answer self-verification SHIPS ON (full-power default)");

        // The operator can turn them off — all three known keys.
        let raw = r#"
            [answers]
            cite = false
            confidence = false
            verify = false
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "answers keys must be known: {issues:?}");
        assert!(!cfg.answers.cite);
        assert!(!cfg.answers.confidence);
        assert!(!cfg.answers.verify);

        // A typo'd answers key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[answers]\nciteee = true\n");
        assert!(
            issues.iter().any(|i| i.contains("answers.citeee")),
            "a typo'd answers key must be reported: {issues:?}"
        );
        // The verify key, too, is spell-checked.
        let (_cfg, issues) = Config::parse("[answers]\nverifyy = true\n");
        assert!(
            issues.iter().any(|i| i.contains("answers.verifyy")),
            "a typo'd verify key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [audit] ships enabled=TRUE (default-on-but-bounded
    /// read-only accountability — a record-only ledger loosens nothing, the SAME
    /// posture as [episodic]), with max_entries defaulting to the audit module's
    /// cap. Both keys parse without an unknown-key diagnostic, and a typo is
    /// diagnosed. With it false the chokepoints behave byte-for-byte as today
    /// (enforced in audit.rs / anthropic.rs); this only pins the gate + spelling.
    #[test]
    fn audit_defaults_on_and_bounded_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.audit.enabled, "the audit log must ship ON (read-only accountability)");
        assert_eq!(
            cfg.audit.max_entries,
            crate::audit::MAX_ENTRIES,
            "the default retention cap is the audit module's bound"
        );

        let raw = r#"
            [audit]
            enabled = false
            max_entries = 500
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "audit keys must be known: {issues:?}");
        assert!(!cfg.audit.enabled);
        assert_eq!(cfg.audit.max_entries, 500);

        let (_cfg, issues) = Config::parse("[audit]\nenable = true\n");
        assert!(
            issues.iter().any(|i| i.contains("audit.enable")),
            "a typo'd audit key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [policy] ships enabled=TRUE but the rule store ships
    /// EMPTY (the rules live in the user-owned state/policy.json, NOT this TOML),
    /// so the layer is INERT by default — every action evaluates to Ask, the SAME
    /// behavior as today (ASK/park everywhere). The `enabled` key parses without an
    /// unknown-key diagnostic, and a typo is diagnosed. USER-SET ONLY: the rules
    /// are deliberately NOT a config key, so the model can never reach a policy via
    /// a config edit; with enabled=false the layer is bypassed (every action Ask).
    #[test]
    fn policy_layer_enabled_but_ships_empty_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.policy.enabled, "the policy layer ships ON (but inert while the store is empty)");

        let raw = r#"
            [policy]
            enabled = false
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "the policy.enabled key must be known: {issues:?}");
        assert!(!cfg.policy.enabled);

        // There is deliberately NO rules key in the TOML — a 'rules' key under
        // [policy] is an unknown key (the rules are user-set via state/policy.json,
        // never the model-reachable config), so an attempt to inject rules here is
        // diagnosed and ignored.
        let (_cfg, issues) = Config::parse("[policy]\nrules = [\"allow gmail_send\"]\n");
        assert!(
            issues.iter().any(|i| i.contains("policy.rules")),
            "policy rules are NOT a config key (user-set only via state/policy.json): {issues:?}"
        );
    }

    /// Contract lockstep: [episodic] ships enabled=TRUE (default-on-but-bounded),
    /// the SAME always-on posture as the transcripts table / lifelong-learning
    /// fact loop. (The autonomy subsystems [self_heal]/[forge]/[optimize] also ship
    /// enabled=true, but their gate is mode="propose" — propose -> human-apply —
    /// never "auto"; [voice_id] remains a deliberately OFF fail-closed gate.) The
    /// honest default is documented in EpisodicConfig:
    /// it is bounded (evict-oldest `retention`), redacted, agent-scoped, gated
    /// per-turn, and forgettable, so on-by-default never means "remembers
    /// everything forever". Both keys parse without an unknown-key diagnostic, and
    /// a typo is diagnosed.
    #[test]
    fn episodic_defaults_on_and_bounded_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.episodic.enabled,
            "the episodic store SHIPS ON (same posture as transcripts/lifelong-learning)"
        );
        assert_eq!(cfg.episodic.retention, 5_000, "bounded evict-oldest cap by default");
        assert!(cfg.episodic.retention > 0, "retention must be a real bound, never unbounded");

        // The operator can turn it OFF and retune the bound — both known keys.
        let raw = r#"
            [episodic]
            enabled = false
            retention = 1000
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "episodic keys must be known: {issues:?}");
        assert!(!cfg.episodic.enabled);
        assert_eq!(cfg.episodic.retention, 1000);

        // A typo'd episodic key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[episodic]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("episodic.enabledd")),
            "typo'd episodic key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [notebooks] ships enabled=TRUE with a bounded evict-oldest
    /// `retention` — the SAME always-on-but-bounded posture as [episodic] (a
    /// notebook is a persisted, cited, READ-ONLY record of a research run, not an
    /// autonomy gate). Both keys parse without an unknown-key diagnostic; a typo is
    /// diagnosed; the operator can turn it OFF and retune the bound.
    #[test]
    fn notebooks_default_on_and_bounded_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.notebooks.enabled, "research notebooks SHIP ON (same posture as episodic)");
        assert!(cfg.notebooks.retention > 0, "retention must be a real bound, never unbounded");
        assert_eq!(cfg.notebooks.retention, 500, "bounded evict-oldest entries cap by default");

        let raw = r#"
            [notebooks]
            enabled = false
            retention = 100
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "notebooks keys must be known: {issues:?}");
        assert!(!cfg.notebooks.enabled);
        assert_eq!(cfg.notebooks.retention, 100);

        let (_cfg, issues) = Config::parse("[notebooks]\nretentionn = 1\n");
        assert!(
            issues.iter().any(|i| i.contains("notebooks.retentionn")),
            "typo'd notebooks key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [lifelog] ships enabled=TRUE — the SAME always-on posture
    /// as [episodic] (the digest is a read-only, never-fabricating fold over the
    /// bounded episodic store; it owns no store, so its only bound is the episodic
    /// bound). The key parses without an unknown-key diagnostic; a typo is diagnosed.
    #[test]
    fn lifelog_defaults_on_and_key_matches_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.lifelog.enabled, "the life-log digest SHIPS ON (read-only over episodic)");

        let (cfg, issues) = Config::parse("[lifelog]\nenabled = false\n");
        assert!(issues.is_empty(), "the lifelog.enabled key must be known: {issues:?}");
        assert!(!cfg.lifelog.enabled);

        let (_cfg, issues) = Config::parse("[lifelog]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("lifelog.enabledd")),
            "typo'd lifelog key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [forge] ships enabled=TRUE (full-power default; inert
    /// without a cloud key), mode="propose" (KEPT — no auto-DEPLOY), the SAME shape as
    /// [self_heal] — and both keys parse without unknown-key diagnostics. (The "no
    /// auto-DEPLOY" guarantee is enforced in forge.rs, not config; this only pins the
    /// gate.)
    #[test]
    fn forge_defaults_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.forge.enabled, "self-forge SHIPS ON (full-power default; inert without a cloud key)");
        assert_eq!(cfg.forge.mode, "propose", "forge stays PROPOSE (the gate; never auto-deploy)");

        let raw = r#"
            [forge]
            enabled = true
            mode = "auto"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "forge keys must be known: {issues:?}");
        assert!(cfg.forge.enabled);
        assert_eq!(cfg.forge.mode, "auto");

        // A typo'd forge key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[forge]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("forge.enabledd")),
            "typo'd forge key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [standing] ships enabled=TRUE (full-power default) — the
    /// Standing-Missions subsystem master switch is ON, but every consequential step a
    /// run takes still parks behind the gate + allow_consequential (no silent
    /// autonomy) — and the key parses without an unknown-key diagnostic. A typo'd key
    /// is diagnosed.
    #[test]
    fn standing_enabled_defaults_on_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.standing.enabled,
            "standing missions SHIP ON (full-power default; every consequential step still parks)"
        );

        let raw = r#"
            [standing]
            enabled = false
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "standing.enabled must be a known key: {issues:?}");
        assert!(!cfg.standing.enabled, "the operator can turn standing missions off");

        // A typo'd standing key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[standing]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("standing.enabledd")),
            "typo'd standing key must be reported: {issues:?}"
        );
    }

    /// TRIPWIRE (condition-trigger) config: the evaluation cadence + anti-flap
    /// re-fire debounce default conservatively, parse as known keys, and are
    /// operator-overridable; a typo is diagnosed.
    #[test]
    fn standing_tripwire_keys_default_and_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert_eq!(cfg.standing.condition_eval_secs, 60, "eval cadence defaults to 60s");
        assert_eq!(cfg.standing.condition_debounce_secs, 3600, "re-fire debounce defaults to 1h");

        let raw = r#"
            [standing]
            condition_eval_secs = 30
            condition_debounce_secs = 1800
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "tripwire keys must be known: {issues:?}");
        assert_eq!(cfg.standing.condition_eval_secs, 30);
        assert_eq!(cfg.standing.condition_debounce_secs, 1800);

        // A typo'd tripwire key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[standing]\ncondition_evl_secs = 30\n");
        assert!(
            issues.iter().any(|i| i.contains("standing.condition_evl_secs")),
            "typo'd tripwire key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (#25): [drafts] ships enabled=TRUE (full-power default) —
    /// proactive drafting is ON. A draft is always a reviewable suggestion (the module
    /// has no send path), so the flag never enables an autonomous send. Keys parse
    /// without an unknown-key diagnostic; a typo is diagnosed.
    #[test]
    fn drafts_enabled_defaults_on_and_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.drafts.enabled, "proactive drafting SHIPS ON (full-power default; no send path)");
        assert_eq!(cfg.drafts.retention, crate::drafts::DEFAULT_RETENTION);

        let raw = "[drafts]\nenabled = false\nretention = 10\n";
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "drafts keys must be known: {issues:?}");
        assert!(!cfg.drafts.enabled, "the operator can turn proactive drafting off");
        assert_eq!(cfg.drafts.retention, 10);

        let (_cfg, issues) = Config::parse("[drafts]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("drafts.enabledd")),
            "typo'd drafts key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (#26): [missions] ships durable=TRUE (full-power default) —
    /// durable persistence is ON. A persisted mission loads PAUSED and re-gates on
    /// resume; the flag governs persistence only, never autonomy. Keys parse cleanly;
    /// a typo is diagnosed.
    #[test]
    fn missions_durable_defaults_on_and_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.missions.durable, "durable missions SHIP ON (full-power default; load PAUSED, re-gate on resume)");
        assert_eq!(cfg.missions.retention, crate::durable_missions::DEFAULT_RETENTION);

        let raw = "[missions]\ndurable = false\nretention = 5\n";
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "missions keys must be known: {issues:?}");
        assert!(!cfg.missions.durable, "the operator can turn durable persistence off");
        assert_eq!(cfg.missions.retention, 5);

        let (_cfg, issues) = Config::parse("[missions]\ndurablee = true\n");
        assert!(
            issues.iter().any(|i| i.contains("missions.durablee")),
            "typo'd missions key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (#27): [macros] ships enabled=TRUE (full-power default) —
    /// macro record/replay is ON. Replay re-runs each command through the router + the
    /// gate FRESH; the store holds only utterances + intent names (never a secret).
    /// Keys parse cleanly; a typo is diagnosed.
    #[test]
    fn macros_enabled_defaults_on_and_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.macros.enabled, "macros SHIP ON (full-power default; replay re-gates each step)");
        assert_eq!(cfg.macros.max_steps, crate::macros::DEFAULT_MAX_STEPS);
        assert_eq!(cfg.macros.retention, crate::macros::DEFAULT_RETENTION);

        let raw = "[macros]\nenabled = false\nmax_steps = 4\nretention = 7\n";
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "macros keys must be known: {issues:?}");
        assert!(!cfg.macros.enabled, "the operator can turn macros off");
        assert_eq!(cfg.macros.max_steps, 4);
        assert_eq!(cfg.macros.retention, 7);

        let (_cfg, issues) = Config::parse("[macros]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("macros.enabledd")),
            "typo'd macros key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [security].encrypt_memory ships FALSE (pinned) — at-rest
    /// encryption is OFF by default, exactly like self_heal/forge/standing/mcp/
    /// optimize/voice_id/docsearch. With it off every store opens plaintext
    /// (byte-for-byte today's behavior); the key parses without an unknown-key
    /// diagnostic and a typo is reported.
    #[test]
    fn security_encrypt_memory_defaults_off_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.security.encrypt_memory,
            "at-rest encryption must ship OFF (enabling changes the on-disk format)"
        );

        let raw = r#"
            [security]
            encrypt_memory = true
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "security.encrypt_memory must be a known key: {issues:?}");
        assert!(cfg.security.encrypt_memory);

        // A typo'd security key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[security]\nencrypt_memoryy = true\n");
        assert!(
            issues.iter().any(|i| i.contains("security.encrypt_memoryy")),
            "typo'd security key must be reported: {issues:?}"
        );
    }

    #[test]
    fn distill_ships_off_parses_fully_and_catches_a_typo() {
        // Ships OFF (training mutates weights — a deliberate opt-in), with the
        // full-power interpreter/model/iters defaults.
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.distill.enabled, "self-distillation must ship OFF");
        assert_eq!(cfg.distill.python, "python3");
        assert!(cfg.distill.base_model.contains("Qwen3-4B"));
        assert_eq!(cfg.distill.iters, 200);

        let raw = r#"
            [distill]
            enabled = true
            python = "/opt/venv/bin/python"
            base_model = "mlx-community/Custom-8B"
            iters = 400
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "distill keys must be known: {issues:?}");
        assert!(cfg.distill.enabled);
        assert_eq!(cfg.distill.python, "/opt/venv/bin/python");
        assert_eq!(cfg.distill.iters, 400);

        // A typo'd distill key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[distill]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("distill.enabledd")),
            "typo'd distill key must be reported: {issues:?}"
        );
    }

    #[test]
    fn scene_ships_off_parses_fully_and_catches_a_typo() {
        // Ships OFF (continuous ambient classification is a privacy opt-in), with a
        // conservative confidence floor.
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.scene.enabled, "acoustic scene awareness must ship OFF");
        assert_eq!(cfg.scene.confidence_floor, 0.6);

        let raw = r#"
            [scene]
            enabled = true
            confidence_floor = 0.8
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "scene keys must be known: {issues:?}");
        assert!(cfg.scene.enabled);
        assert_eq!(cfg.scene.confidence_floor, 0.8);

        // A typo'd scene key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[scene]\nconfidence_flooor = 0.9\n");
        assert!(
            issues.iter().any(|i| i.contains("scene.confidence_flooor")),
            "typo'd scene key must be reported: {issues:?}"
        );
    }

    #[test]
    fn load_live_keeps_last_good_on_transient_failures_but_loads_valid_files() {
        // The live-reload reader must NEVER hand back fabricated defaults for a
        // transiently unreadable/truncated file (the audit-snapshot emitters
        // would blip every panel for a tick) — None means "keep last good".
        let dir = std::env::temp_dir().join(format!("darwin-loadlive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("darwin.toml");

        // Missing file -> None (a restart applies defaults; a live blip never does).
        assert!(Config::load_live(&path).is_none());
        // Empty file (a truncate-then-write save window) -> None.
        std::fs::write(&path, "").unwrap();
        assert!(Config::load_live(&path).is_none());
        // Mid-save partial write / TOML syntax error -> None.
        std::fs::write(&path, "[overnight]\nenabled = tr").unwrap();
        assert!(Config::load_live(&path).is_none());
        // A valid file loads, and a flipped switch is visible.
        std::fs::write(&path, "[overnight]\nenabled = true\n").unwrap();
        assert!(Config::load_live(&path).unwrap().overnight.enabled);
        // Per-key warnings still load (identical to boot): the typo'd key is
        // ignored but the file's other values apply.
        std::fs::write(&path, "[overnight]\nenabledd = true\n\n[scene]\nenabled = true\n").unwrap();
        let cfg = Config::load_live(&path).expect("valid TOML with a warning still loads");
        assert!(cfg.scene.enabled);
        assert!(!cfg.overnight.enabled, "the typo'd key is a warning, not a flip");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overnight_ships_off_parses_fully_and_catches_a_typo() {
        // Ships OFF (autonomous unattended work is opt-in), with a once-per-night gap.
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.overnight.enabled, "overnight agents must ship OFF");
        assert_eq!(cfg.overnight.min_gap_secs, 6 * 3600);

        let raw = r#"
            [overnight]
            enabled = true
            min_gap_secs = 3600
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "overnight keys must be known: {issues:?}");
        assert!(cfg.overnight.enabled);
        assert_eq!(cfg.overnight.min_gap_secs, 3600);

        // A typo'd overnight key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[overnight]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("overnight.enabledd")),
            "typo'd overnight key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [voice_id] ships enabled=false — speaker verification
    /// is the one deliberate OFF default (a fail-closed GATE, not a full-power
    /// feature; enrollment is always explicit), and every key parses without an
    /// unknown-key diagnostic. With it off (or no enrolled profile) NOTHING is gated
    /// by voice.
    #[test]
    fn voice_id_defaults_off_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.voice_id.enabled, "voice-id must ship OFF (no silent voice gating)");
        // Sensible, finite defaults.
        assert!(cfg.voice_id.threshold > 0.0 && cfg.voice_id.threshold < 1.0, "threshold in (0,1)");
        assert!(cfg.voice_id.min_enroll_samples >= 1, "needs at least one enroll sample");
        assert_eq!(cfg.voice_id.gate_scope, "consequential", "default gates consequential only");

        // All keys parse as known and round-trip.
        let raw = r#"
            [voice_id]
            enabled = true
            threshold = 0.9
            min_enroll_samples = 5
            gate_scope = "all"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "voice_id keys must be known: {issues:?}");
        assert!(cfg.voice_id.enabled);
        assert!((cfg.voice_id.threshold - 0.9).abs() < 1e-12);
        assert_eq!(cfg.voice_id.min_enroll_samples, 5);
        assert_eq!(cfg.voice_id.gate_scope, "all");

        // A typo'd voice_id key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[voice_id]\nthreshholdd = 0.5\n");
        assert!(
            issues.iter().any(|i| i.contains("voice_id.threshholdd")),
            "typo'd voice_id key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [docsearch] ships enabled=TRUE (full-power default) AND
    /// roots=[] — on-device file RAG is ON but INERT WITHOUT ROOTS: the empty
    /// allowlist means "index nothing" until the user allowlists a folder (the
    /// installer must NOT guess). Every key parses without an unknown-key diagnostic,
    /// every bound is finite (never unbounded), and a typo is diagnosed.
    #[test]
    fn docsearch_defaults_on_empty_roots_and_bounded() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.docsearch.enabled, "file RAG SHIPS ON (full-power default; inert without roots)");
        assert!(cfg.docsearch.roots.is_empty(), "no folder is indexable by default (no whole-disk scan; installer must not guess)");
        // Every bound is a real, finite ceiling — never unbounded.
        assert!(cfg.docsearch.max_files > 0, "max_files must be a real bound");
        assert!(cfg.docsearch.max_chunks > 0, "max_chunks must be a real bound");
        assert!(cfg.docsearch.max_file_bytes > 0, "max_file_bytes must be a real bound");
        assert!(cfg.docsearch.max_depth > 0, "max_depth must be a real bound");
        assert!(cfg.docsearch.chunk_chars > 0, "chunk_chars must be a real bound");
        assert!(
            cfg.docsearch.chunk_overlap < cfg.docsearch.chunk_chars,
            "overlap must be smaller than the chunk window or chunking never advances"
        );

        // The operator can turn it on, allowlist a root, and retune the bounds —
        // all known keys, all round-tripping.
        let raw = r#"
            [docsearch]
            enabled = true
            roots = ["/Users/me/Documents", "/Users/me/notes"]
            max_files = 100
            max_chunks = 1000
            max_file_bytes = 65536
            max_depth = 4
            chunk_chars = 800
            chunk_overlap = 100
            build_graph = true
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "docsearch keys must all be known: {issues:?}");
        assert!(cfg.docsearch.enabled);
        assert_eq!(cfg.docsearch.roots, vec!["/Users/me/Documents", "/Users/me/notes"]);
        assert_eq!(cfg.docsearch.max_files, 100);
        assert_eq!(cfg.docsearch.max_chunks, 1000);
        assert_eq!(cfg.docsearch.max_file_bytes, 65536);
        assert_eq!(cfg.docsearch.max_depth, 4);
        assert_eq!(cfg.docsearch.chunk_chars, 800);
        assert_eq!(cfg.docsearch.chunk_overlap, 100);
        // `build_graph` is a real parsed field, so it must round-trip AND be a known
        // key (no false "unknown config key docsearch.build_graph ignored").
        assert!(cfg.docsearch.build_graph);

        // A typo'd docsearch key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[docsearch]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("docsearch.enabledd")),
            "typo'd docsearch key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (task #16): [code] ships enabled=TRUE (full-power default)
    /// AND roots=[] — code intelligence (code_explain + code_propose_diff) is ON but
    /// INERT WITHOUT ROOTS: an empty `roots` allowlist means "no codebase is
    /// reachable" until the user allowlists one (the installer must NOT guess). Every
    /// key parses without an unknown-key diagnostic, the bound is finite, and a typo is
    /// diagnosed.
    #[test]
    fn code_defaults_on_empty_roots_and_bounded() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.code.enabled,
            "code intelligence SHIPS ON (full-power default; inert without an allowlisted root)"
        );
        assert!(
            cfg.code.roots.is_empty(),
            "no codebase is reachable by default (never an arbitrary path; installer must not guess)"
        );
        assert!(cfg.code.max_diff_bytes > 0, "max_diff_bytes must be a real bound");

        // The operator can turn it on, allowlist a codebase root, and retune the
        // bound — all known keys, all round-tripping.
        let raw = r#"
            [code]
            enabled = true
            roots = ["/Users/me/proj", "/Users/me/other"]
            max_diff_bytes = 4096
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "code keys must all be known: {issues:?}");
        assert!(cfg.code.enabled);
        assert_eq!(cfg.code.roots, vec!["/Users/me/proj", "/Users/me/other"]);
        assert_eq!(cfg.code.max_diff_bytes, 4096);

        // A typo'd code key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[code]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("code.enabledd")),
            "typo'd code key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [shell] (the sandboxed shell / terminal #43, the
    /// HIGHEST-RISK capability) SHIPS ON (enabled=true, full-power default) but NEVER
    /// auto-runs and is INERT WITHOUT device support (/usr/bin/sandbox-exec + /bin/sh).
    /// With it off the shell intent is never classified and `shell_run` is inert.
    /// Even ON, every command parks for a spoken yes + clears the denylist +
    /// the master switch + voice-id + !lockdown. Every key parses without an
    /// unknown-key diagnostic, and a typo is diagnosed.
    #[test]
    fn shell_defaults_on_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.shell.enabled,
            "the sandboxed shell SHIPS ON (full-power default) — even ON it never auto-runs (parks per-action; device-gated exec)"
        );
        // It is ON-by-default identically to the struct's Default.
        let defaults = Config::default();
        assert_eq!(cfg.shell.enabled, defaults.shell.enabled);

        // The operator can deliberately disable it — a known, round-tripping key.
        let (cfg, issues) = Config::parse("[shell]\nenabled = false\n");
        assert!(issues.is_empty(), "shell keys must all be known: {issues:?}");
        assert!(!cfg.shell.enabled, "operator-disabled shell parses false");

        // A typo'd shell key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[shell]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("shell.enabledd")),
            "typo'd shell key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [ui_automation] (gated UI automation #44, the CAPSTONE —
    /// the SINGLE MOST DANGEROUS capability, physically actuating the macOS UI)
    /// SHIPS ON (enabled=true, full-power default) but NEVER auto-runs and is INERT
    /// WITHOUT Accessibility TCC consent + a real display.
    /// With it off the actuate intent is never classified and `ui_actuate` is inert.
    /// Even ON, every actuation parks
    /// PER ACTION for a spoken yes + clears the master switch + voice-id + !lockdown,
    /// and the actuation itself is device-gated behind the Accessibility TCC consent.
    /// Every key parses without an unknown-key diagnostic, and a typo is diagnosed.
    #[test]
    fn ui_automation_defaults_on_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.ui_automation.enabled,
            "gated UI automation SHIPS ON (full-power default) — even ON it never auto-runs (parks PER ACTION; inert without Accessibility TCC)"
        );
        // It is ON-by-default identically to the struct's Default.
        let defaults = Config::default();
        assert_eq!(cfg.ui_automation.enabled, defaults.ui_automation.enabled);
        assert!(defaults.ui_automation.enabled, "the struct default is ON");

        // The operator can deliberately disable it — a known, round-tripping key.
        let (cfg, issues) = Config::parse("[ui_automation]\nenabled = false\n");
        assert!(issues.is_empty(), "ui_automation keys must all be known: {issues:?}");
        assert!(!cfg.ui_automation.enabled, "operator-disabled ui_automation parses false");

        // actuate_via_app SHIPS OFF (default false): the existing LOCAL CGEvent post
        // is the default, byte-for-byte unchanged. It is opt-in only.
        assert!(
            !cfg.ui_automation.actuate_via_app,
            "actuate_via_app SHIPS OFF — the default is the local CGEvent post"
        );
        assert_eq!(
            cfg.ui_automation.actuate_via_app,
            defaults.ui_automation.actuate_via_app
        );
        assert!(
            !defaults.ui_automation.actuate_via_app,
            "the struct default for actuate_via_app is OFF"
        );

        // The operator can opt in to posting the actuation THROUGH the HUD app — a
        // known, round-tripping bool key.
        let (cfg, issues) =
            Config::parse("[ui_automation]\nactuate_via_app = true\n");
        assert!(
            issues.is_empty(),
            "ui_automation keys must all be known: {issues:?}"
        );
        assert!(
            cfg.ui_automation.actuate_via_app,
            "operator-enabled actuate_via_app parses true"
        );
        // Opting in to the HUD post path does NOT change the master enable default.
        assert!(
            cfg.ui_automation.enabled,
            "enabling actuate_via_app leaves the master switch at its ON default"
        );

        // A typo'd ui_automation key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[ui_automation]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("ui_automation.enabledd")),
            "typo'd ui_automation key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [vision] (the on-device VLM describe path) SHIPS ON
    /// (full-power default) but is INERT WITHOUT A MODEL (the model ships EMPTY).
    /// With an empty model, the "describe my screen / what am I looking at / describe
    /// this image" intent honestly reports unavailable and falls back. The operator
    /// names a (downloaded) model deliberately to engage it. Every key parses without
    /// an unknown-key diagnostic, and a typo is diagnosed.
    #[test]
    fn vision_vlm_defaults_on_empty_model_and_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.vision.enabled,
            "the on-device VLM describe path SHIPS ON (full-power default; inert without a downloaded model)"
        );
        assert!(
            cfg.vision.model.is_empty(),
            "no VLM is named by default — empty model means the op honestly reports unavailable"
        );

        // The operator can turn it on and name a model — both known keys, round-tripping.
        let raw = r#"
            [vision]
            enabled = true
            model = "mlx-community/Qwen2-VL-2B-Instruct-4bit"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "vision keys must all be known: {issues:?}");
        assert!(cfg.vision.enabled);
        assert_eq!(cfg.vision.model, "mlx-community/Qwen2-VL-2B-Instruct-4bit");

        // A typo'd vision key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[vision]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("vision.enabledd")),
            "typo'd vision key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (task #18): [image] (the on-device text->image
    /// generation path) SHIPS ON (full-power default) but is INERT WITHOUT A MODEL
    /// (the model ships EMPTY). With an empty model, the "generate/make/draw an image
    /// of X" intent surfaces an honest "not set up" line. The operator names a
    /// (downloaded) diffusion model deliberately to engage it. Every key parses
    /// without an unknown-key diagnostic, and a typo is diagnosed. HONESTY: image
    /// generation is LOCAL only (MLX diffusion; the prompt + pixels stay on-device,
    /// NO cloud image API) — the empty-model default keeps the multi-GB model gated.
    #[test]
    fn image_gen_defaults_on_empty_model_and_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.image.enabled,
            "the on-device image-generation path SHIPS ON (full-power default; inert without a downloaded diffusion model)"
        );
        assert!(
            cfg.image.model.is_empty(),
            "no image model is named by default — empty model means the op honestly reports unavailable"
        );

        // The operator can turn it on and name a model — both known keys, round-tripping.
        let raw = r#"
            [image]
            enabled = true
            model = "schnell"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "image keys must all be known: {issues:?}");
        assert!(cfg.image.enabled);
        assert_eq!(cfg.image.model, "schnell");

        // A typo'd image key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[image]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("image.enabledd")),
            "typo'd image key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (task #15): [audio].sound_monitor — the ambient sound
    /// monitor — SHIPS ON (true, full-power default) but is INERT WITHOUT
    /// Microphone/TCC consent (the flag cannot grant it, so it captures nothing
    /// until consent is granted). With it OFF the audio path is byte-for-byte today's (the
    /// one-shot "what was that sound" intent on an already-captured clip needs no
    /// switch). The operator turns it on deliberately. Every key parses without an
    /// unknown-key diagnostic, and a typo is diagnosed. The other audio knobs keep
    /// their defaults (the new field is additive — it must not perturb them).
    #[test]
    fn sound_monitor_ships_on_and_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.audio.sound_monitor,
            "the ambient sound monitor SHIPS ON (full-power default; inert without Microphone/TCC consent)"
        );
        // Additive: the rest of the audio contract is untouched by the new field.
        assert_eq!(cfg.audio.rms_threshold, 0.015);
        assert_eq!(cfg.audio.silence_ms, 350);
        assert_eq!(cfg.audio.min_speech_ms, 250);
        assert!(cfg.audio.barge_in);
        assert_eq!(cfg.audio.barge_in_rms, 0.06);
        assert_eq!(cfg.audio.barge_in_ms, 250);

        // The operator can opt out — a known key, round-tripping, leaving the rest.
        let (cfg, issues) = Config::parse("[audio]\nsound_monitor = false\n");
        assert!(issues.is_empty(), "sound_monitor must be a known key: {issues:?}");
        assert!(!cfg.audio.sound_monitor, "the operator can deliberately opt out");
        assert_eq!(cfg.audio.rms_threshold, 0.015, "the other audio knobs keep their defaults");

        // A typo'd key is diagnosed, not silently swallowed (so a misspelled opt-out
        // never silently leaves the monitor in an unexpected state).
        let (cfg, issues) = Config::parse("[audio]\nsound_moniter = false\n");
        assert!(
            issues.iter().any(|i| i.contains("audio.sound_moniter")),
            "typo'd sound_monitor key must be reported: {issues:?}"
        );
        assert!(cfg.audio.sound_monitor, "a typo'd opt-out never silently disarms the monitor (it keeps the ON default)");
    }

    /// Contract lockstep: [voice] (the ElevenLabs cloud voice tier) SHIPS ON
    /// (cloud_tier=true, full-power default) but is INERT WITHOUT A KEY — reached only
    /// when an elevenlabs key is present AND the tier is non-Local; otherwise TTS is
    /// the on-device Kokoro default (also the fallback on any EL error). The default
    /// model is eleven_flash_v2_5 and the per-agent voice map is empty (so every agent
    /// uses its Kokoro voice until mapped). Every key parses without an unknown-key diagnostic.
    #[test]
    fn voice_tier_ships_on_inert_without_key_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.voice.cloud_tier,
            "the ElevenLabs cloud voice tier SHIPS ON (full-power default; INERT WITHOUT A KEY — Kokoro stays the fallback)"
        );
        assert!(
            cfg.voice.cloud_stt,
            "the ElevenLabs Scribe cloud-STT tier SHIPS ON (full-power default; INERT WITHOUT A KEY — on-device whisper stays the fallback)"
        );
        assert!(
            cfg.voice.diarize,
            "#31 multi-speaker diarization SHIPS ON (full-power default; INERT ON-DEVICE — honest single-stream without EL Scribe)"
        );
        assert!(cfg.voice.adaptive_prosody, "#33 adaptive prosody SHIPS ON (full-power default)");
        assert!(cfg.voice.whisper, "#34 whisper mode SHIPS ON (full-power default)");
        assert!(cfg.voice.whisper_auto, "#34 whisper auto-engage SHIPS ON (full-power default)");
        assert!(
            cfg.voice.cloud_sfx,
            "the sound-effect cue tier SHIPS ON (full-power default; INERT WITHOUT A KEY — silent no-op without the EL key, no on-device SFX generator)"
        );
        assert!(
            cfg.voice.cloud_music,
            "the music-generation tier SHIPS ON (full-power default; INERT WITHOUT A KEY — honest unavailable without the EL key, no on-device music generator)"
        );
        assert!(
            !cfg.voice.stream_tts,
            "low-latency streaming TTS is OPT-IN (ships OFF) — default behavior is unchanged (today's blocking synthesis)"
        );
        assert!(
            cfg.voice.pronunciation_dictionary_id.is_empty(),
            "no active pronunciation dictionary by default (empty = no speak locator)"
        );
        assert!(
            cfg.voice.pronunciation_dictionary_version.is_empty(),
            "no pronunciation-dictionary version pinned by default (empty = latest)"
        );
        assert!(
            !cfg.voice.event_cues,
            "event cues are OPT-IN (ship OFF) — DEFAULT BEHAVIOR IS UNCHANGED: no cue is spawned on confirm/deny"
        );
        assert_eq!(cfg.voice.model, "eleven_flash_v2_5", "default EL model");
        assert_eq!(
            cfg.voice.voices.get("darwin").map(String::as_str),
            Some("JBFqnCBsd6RMkjVDRZzb"),
            "Darwin-Prime ships mapped to the ElevenLabs premade 'George' (British) voice so the cloud tier engages with just a key"
        );
        assert_eq!(
            cfg.voice.mic_source, "device",
            "mic_source DEFAULTS to \"device\" (today's cpal capture, byte-for-byte) — \"app\" is opt-in"
        );

        // All keys parse as known and round-trip (including the [voice.voices] map).
        let raw = r#"
            [voice]
            cloud_tier = false
            cloud_stt = false
            diarize = false
            cloud_sfx = false
            cloud_music = false
            stream_tts = true
            event_cues = true
            mic_source = "app"
            pronunciation_dictionary_id = "EL_PD_ID"
            pronunciation_dictionary_version = "EL_PD_VER"
            model = "eleven_multilingual_v2"

            [voice.voices]
            darwin = "EL_VOICE_DARWIN"
            friday = "EL_VOICE_FRIDAY"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "voice keys must be known: {issues:?}");
        assert!(!cfg.voice.cloud_tier, "the operator can turn the cloud TTS tier off");
        assert!(!cfg.voice.cloud_stt, "cloud_stt must round-trip as a known key");
        assert!(!cfg.voice.diarize, "diarize must round-trip as a known key");
        assert!(!cfg.voice.cloud_sfx, "cloud_sfx must round-trip as a known key (operator can turn the SFX cue tier off)");
        assert!(!cfg.voice.cloud_music, "cloud_music must round-trip as a known key (operator can turn the music-generation tier off)");
        assert!(cfg.voice.stream_tts, "stream_tts must round-trip as a known key (operator can opt in to streaming TTS)");
        assert!(cfg.voice.event_cues, "event_cues must round-trip as a known key (operator can opt in to event cues)");
        assert_eq!(cfg.voice.mic_source, "app", "mic_source must round-trip as a known key (operator can route the mic in from the HUD app)");
        assert_eq!(
            cfg.voice.pronunciation_dictionary_id, "EL_PD_ID",
            "pronunciation_dictionary_id must round-trip as a known key"
        );
        assert_eq!(
            cfg.voice.pronunciation_dictionary_version, "EL_PD_VER",
            "pronunciation_dictionary_version must round-trip as a known key"
        );
        assert_eq!(cfg.voice.model, "eleven_multilingual_v2");
        assert_eq!(cfg.voice.voices.get("darwin").map(String::as_str), Some("EL_VOICE_DARWIN"));
        assert_eq!(cfg.voice.voices.get("friday").map(String::as_str), Some("EL_VOICE_FRIDAY"));

        // A typo'd voice key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[voice]\ncloud_tierr = true\n");
        assert!(
            issues.iter().any(|i| i.contains("voice.cloud_tierr")),
            "typo'd voice key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [wake] (#32 custom wake-word) SHIPS ON (enabled=true,
    /// full-power default) and the default phrase is "darwin" — so enabling preserves
    /// today's activation behavior exactly (identical unless the phrase is changed).
    /// Every key parses without an unknown-key diagnostic.
    #[test]
    fn wake_ships_on_default_phrase_darwin_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.wake.enabled, "custom wake-word gating SHIPS ON (full-power default; phrase 'darwin' = today's behavior)");
        assert_eq!(
            cfg.wake.phrase, "darwin",
            "the default wake phrase preserves today's activation behavior"
        );

        // Both keys parse as known and round-trip.
        let raw = r#"
            [wake]
            enabled = false
            phrase = "computer"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "wake keys must be known: {issues:?}");
        assert!(!cfg.wake.enabled, "the operator can turn wake-word gating off");
        assert_eq!(cfg.wake.phrase, "computer");

        // A typo'd wake key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[wake]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("wake.enabledd")),
            "typo'd wake key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [interpret] (#30 continuous live interpretation) ships
    /// live=TRUE (full-power default; INERT WITHOUT TCC/MIC) and speak=false (voicing
    /// the translation stays its OWN opt-in, render-only). The default target is
    /// "English" and the source auto-detects (empty). Every key parses without an
    /// unknown-key diagnostic.
    #[test]
    fn interpret_ships_on_inert_without_mic_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.interpret.live, "continuous live interpretation SHIPS ON (full-power default; inert without mic/TCC)");
        assert!(!cfg.interpret.speak, "voicing the translation stays its OWN opt-in (render-only default)");
        assert_eq!(cfg.interpret.target_lang, "English", "default target language");
        assert_eq!(cfg.interpret.source_lang, "", "empty source => auto-detect");

        // All keys parse as known and round-trip.
        let raw = r#"
            [interpret]
            live = false
            speak = true
            source_lang = "Spanish"
            target_lang = "English"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "interpret keys must be known: {issues:?}");
        assert!(!cfg.interpret.live, "the operator can turn live interpretation off");
        assert!(cfg.interpret.speak);
        assert_eq!(cfg.interpret.source_lang, "Spanish");
        assert_eq!(cfg.interpret.target_lang, "English");

        // A typo'd interpret key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[interpret]\nlivee = true\n");
        assert!(
            issues.iter().any(|i| i.contains("interpret.livee")),
            "typo'd interpret key must be reported: {issues:?}"
        );
    }

    /// [skills].enabled DEFAULTS ON — UNLIKE self_heal/forge/standing/mcp, the
    /// pure in-tree skill library is safe to offer by default. The key parses as a
    /// known key, can be turned off, and a typo is diagnosed.
    #[test]
    fn skills_enabled_defaults_on_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.skills.enabled,
            "the pure skill library ships ON (safe by default)"
        );

        // Explicitly turning it off is honored without a diagnostic.
        let (cfg, issues) = Config::parse("[skills]\nenabled = false\n");
        assert!(issues.is_empty(), "skills.enabled must be a known key: {issues:?}");
        assert!(!cfg.skills.enabled, "operator can turn the library off");

        // A typo'd skills key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[skills]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("skills.enabledd")),
            "typo'd skills key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [integrations] ships allow_consequential=TRUE (full-power
    /// default) — THE master gate for outward actions is ARMED, but INERT-SAFE: a
    /// CONFIRMED consequential action still clears confirm + voice-id + policy +
    /// !lockdown at the runtime chokepoints (this flag only decides run-for-real vs.
    /// DryRun preview). The key parses without an unknown-key diagnostic.
    #[test]
    fn integrations_allow_consequential_defaults_on_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.integrations.allow_consequential,
            "the consequential master gate SHIPS ON (full-power default; ARMED but still per-action gated)"
        );

        let raw = r#"
            [integrations]
            allow_consequential = false
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "allow_consequential must be a known key: {issues:?}");
        assert!(!cfg.integrations.allow_consequential, "the operator can disarm the master gate");
    }

    /// Contract lockstep: [mcp] ships enabled=TRUE (full-power default) — the MCP
    /// subsystem (external tool servers) is ON but INERT WITHOUT SERVERS: it ships
    /// with NO servers configured (the installer must NOT add any), so nothing
    /// connects until the user adds one, with safe-but-finite bounds. The keys parse
    /// without an unknown-key diagnostic.
    #[test]
    fn mcp_defaults_on_with_no_servers_and_finite_bounds() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.mcp.enabled, "MCP SHIPS ON (full-power default; inert without servers)");
        assert!(cfg.mcp.servers.is_empty(), "MCP must ship with no servers (installer must not add any)");
        assert!(cfg.mcp.max_servers > 0 && cfg.mcp.max_servers < 1000, "finite server bound");
        assert!(cfg.mcp.max_tools_per_server > 0, "finite tool bound");
        assert!(cfg.mcp.call_timeout_ms > 0, "finite call timeout");
        assert!(cfg.mcp.max_output_bytes > 0, "finite output cap");
    }

    /// The [mcp] top-level keys and a full [[mcp.servers]] entry parse cleanly,
    /// classification + transport enums deserialize, and the per-entry
    /// `deny_unknown_fields` rejects a typo'd server key (it falls the SECTION
    /// back to defaults with an issue, never silently widening the surface).
    #[test]
    fn mcp_full_server_entry_parses_and_typos_are_caught() {
        let raw = r#"
            [mcp]
            enabled = true
            max_servers = 3
            call_timeout_ms = 5000

            [[mcp.servers]]
            name = "files"
            transport = "stdio"
            command = "/usr/bin/srv"
            args = ["--root", "/p"]
            uses_token = true
            agents = ["friday"]
            default_class = "consequential"
            read_only_tools = ["list", "read"]
            fs_read = ["/p"]
            net_hosts = []
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "clean [mcp] must parse with no issues: {issues:?}");
        assert!(cfg.mcp.enabled);
        assert_eq!(cfg.mcp.max_servers, 3);
        assert_eq!(cfg.mcp.servers.len(), 1);
        let s = &cfg.mcp.servers[0];
        assert_eq!(s.name, "files");
        assert!(s.uses_token, "uses_token must parse");
        assert_eq!(s.agents, vec!["friday".to_string()]);
        assert_eq!(s.read_only_tools.len(), 2);

        // A typo'd server key must be caught (deny_unknown_fields) — the section
        // falls back to defaults with a reported issue, never silently accepted.
        let raw_bad = r#"
            [mcp]
            enabled = true
            [[mcp.servers]]
            name = "files"
            commnd = "/usr/bin/srv"   # typo: not a known server field
        "#;
        let (cfg, issues) = Config::parse(raw_bad);
        assert!(
            !issues.is_empty(),
            "a typo'd server field must be reported, not silently accepted"
        );
        assert!(cfg.mcp.servers.is_empty(), "the bad section falls back to defaults");
    }

    /// A typo'd top-level [mcp] key is reported (unknown-key diagnostic), not
    /// silently swallowed — the operator must know their bound did not apply.
    #[test]
    fn mcp_typoed_top_level_key_is_reported() {
        let (_cfg, issues) = Config::parse("[mcp]\nmax_serverss = 2\n");
        assert!(
            issues.iter().any(|i| i.contains("mcp.max_serverss")),
            "typo'd [mcp] key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (#35): [webhooks] ships enabled=TRUE (full-power default) but
    /// INERT WITHOUT MAPPINGS + SECRET — it binds 127.0.0.1 loopback by default, has
    /// NO mappings (an unmapped event is rejected), and the secret is NOT in the TOML
    /// (it must be in the Keychain). So even on, nothing is accepted until the user
    /// adds a mapping + sets the secret.
    #[test]
    fn webhooks_default_on_loopback_no_mappings() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.webhooks.enabled, "webhook receiver SHIPS ON (full-power default; inert without mappings + secret)");
        assert_eq!(cfg.webhooks.bind, "127.0.0.1", "must default to loopback");
        assert!(cfg.webhooks.mappings.is_empty(), "no event->intent mappings by default");
        assert!(cfg.webhooks.max_body_bytes > 0, "finite body cap");
        assert!(cfg.webhooks.port > 0, "a listen port");
    }

    /// A full [webhooks] section + a [[webhooks.mappings]] entry parses cleanly;
    /// the per-entry `deny_unknown_fields` catches a typo'd mapping key (the
    /// section falls back, never silently widening the event allowlist).
    #[test]
    fn webhooks_full_section_parses_and_mapping_typos_are_caught() {
        let raw = r#"
            [webhooks]
            enabled = true
            bind = "127.0.0.1"
            port = 9100
            max_body_bytes = 4096

            [[webhooks.mappings]]
            event = "ci.failed"
            intent = "system.query"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "clean [webhooks] must parse: {issues:?}");
        assert!(cfg.webhooks.enabled);
        assert_eq!(cfg.webhooks.port, 9100);
        assert_eq!(cfg.webhooks.mappings.len(), 1);
        assert_eq!(cfg.webhooks.mappings[0].event, "ci.failed");
        assert_eq!(cfg.webhooks.mappings[0].intent, "system.query");

        let raw_bad = r#"
            [webhooks]
            enabled = true
            [[webhooks.mappings]]
            event = "ci.failed"
            intnt = "system.query"   # typo: not a known mapping field
        "#;
        let (cfg, issues) = Config::parse(raw_bad);
        assert!(!issues.is_empty(), "a typo'd mapping field must be reported");
        assert!(cfg.webhooks.mappings.is_empty(), "the bad section falls back to defaults");
    }

    #[test]
    fn introspect_full_section_parses_and_a_typo_is_caught() {
        let raw = r#"
            [introspect]
            enabled = false
            interval_secs = 120
            startup_delay_secs = 5
            cpu_alert_percent = 80.0
            rss_growth_ratio = 2.5
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "clean [introspect] must parse: {issues:?}");
        assert!(!cfg.introspect.enabled);
        assert_eq!(cfg.introspect.interval_secs, 120);
        assert_eq!(cfg.introspect.startup_delay_secs, 5);
        assert_eq!(cfg.introspect.cpu_alert_percent, 80.0);
        assert_eq!(cfg.introspect.rss_growth_ratio, 2.5);

        // An absent section keeps the shipped defaults (unchanged behavior).
        let (def, _) = Config::parse("");
        assert!(def.introspect.enabled);
        assert_eq!(def.introspect.interval_secs, 60);
        assert_eq!(def.introspect.cpu_alert_percent, 95.0);

        // A typo'd key is reported, not silently swallowed.
        let (_c, issues) = Config::parse("[introspect]\ninterval_sec = 30\n");
        assert!(
            issues.iter().any(|i| i.contains("introspect.interval_sec")),
            "a typo'd [introspect] key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [interception] ships enabled=TRUE (full-power default) —
    /// the READ-ONLY "is anything MITMing me?" check is armed. Keys parse cleanly; a
    /// typo is diagnosed; an absent section keeps the shipped defaults.
    #[test]
    fn interception_full_section_parses_and_a_typo_is_caught() {
        let raw = r#"
            [interception]
            enabled = false
            interval_secs = 120
            startup_delay_secs = 5
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "clean [interception] must parse: {issues:?}");
        assert!(!cfg.interception.enabled, "the operator can turn the check off");
        assert_eq!(cfg.interception.interval_secs, 120);
        assert_eq!(cfg.interception.startup_delay_secs, 5);

        // An absent section keeps the shipped ON defaults (read-only observability).
        let (def, _) = Config::parse("");
        assert!(def.interception.enabled, "traffic-interception check SHIPS ON (full-power default)");
        assert_eq!(def.interception.interval_secs, 300);
        assert_eq!(def.interception.startup_delay_secs, 50);

        // A typo'd key is reported, not silently swallowed.
        let (_c, issues) = Config::parse("[interception]\ninterval_sec = 30\n");
        assert!(
            issues.iter().any(|i| i.contains("interception.interval_sec")),
            "a typo'd [interception] key must be reported: {issues:?}"
        );
    }

    /// A typo'd top-level [webhooks] key is reported, not silently swallowed.
    #[test]
    fn webhooks_typoed_top_level_key_is_reported() {
        let (_cfg, issues) = Config::parse("[webhooks]\nbnd = \"127.0.0.1\"\n");
        assert!(
            issues.iter().any(|i| i.contains("webhooks.bnd")),
            "typo'd [webhooks] key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (#36): [plugin_sdk] ships enabled=TRUE (full-power default) —
    /// the live register-on-launch handshake is ON (the pure validator is always
    /// available regardless; a plugin still can't over-privilege itself or escape the
    /// SBPL profile). The key parses without a diagnostic.
    #[test]
    fn plugin_sdk_defaults_on_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.plugin_sdk.enabled, "the plugin-SDK launch handshake SHIPS ON (full-power default)");

        let (cfg, issues) = Config::parse("[plugin_sdk]\nenabled = false\n");
        assert!(issues.is_empty(), "plugin_sdk.enabled must be a known key: {issues:?}");
        assert!(!cfg.plugin_sdk.enabled, "the operator can turn the launch handshake off");
    }
}
