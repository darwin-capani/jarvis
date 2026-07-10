/**
 * Pure HUD state core — telemetry events in, render-ready state out.
 *
 * No DOM, React, three.js, or Tauri imports. Everything here is exercised
 * headlessly by vitest (src/test/state.test.ts). The render layer only ever
 * calls `reduce` and reads the returned snapshot.
 *
 * Anti-flash invariants (user directive #5):
 *  - idle->listening requires ENTER_FRAMES_TO_LISTEN consecutive frames
 *    ABOVE LISTEN_ENTER_RMS (~130ms dwell), listening->idle requires
 *    QUIET_FRAMES_TO_IDLE consecutive frames BELOW LISTEN_EXIT_RMS (~600ms).
 *    The enter/exit gap (0.018 / 0.012) is hysteresis: ambient rms hovering
 *    at the old single 0.015 threshold can never oscillate the state.
 *  - In-band evidence is a keepalive: loud frames while listening and
 *    speaking=true frames while speaking refresh stateSince so the 12s
 *    stale decay never demotes an actively-evidenced state.
 *  - Reducer cases return the SAME state reference whenever nothing
 *    render-visible changed (audio.level fast path, repeated
 *    ws.disconnected) so React can bail out of re-rendering.
 *  - Toasts exit in two phases (exiting -> removed after TOAST_EXIT_MS) so
 *    the view can fade them out instead of single-frame cuts.
 */
import {
  AnswerAnnotation,
  AuditSnapshot,
  CodeExplained,
  CodeProposal,
  ShellOutcome,
  UiActuateOutcome,
  PendingDraft,
  DurableMission,
  MacroEntry,
  DocIndexStatus,
  DocSearchResult,
  EvalReport,
  KnowledgeGraphResult,
  LifeLogDigest,
  LiveGateEvent,
  LocalToolsStatus,
  LocalWarmStatus,
  InferencePerfStatus,
  LockdownStatus,
  CapabilityAtlas,
  TccSentinel,
  IntrospectStatus,
  IntrospectCapability,
  AttributionHealth,
  McpStatus,
  WebhookSurface,
  PluginSurface,
  ModelTierStatus,
  NotebookActivity,
  OptimizerProposal,
  PolicySnapshot,
  SecurityStatus,
  SkillsCatalog,
  SttTierStatus,
  AudioIoStatus,
  Suggestion,
  ProactiveDigest,
  FocusActive,
  TelemetryEnvelope,
  UnifiedSearchResult,
  VerifyStatus,
  CrossCheckStatus,
  DebateStatus,
  VisionDescribe,
  AudioSoundMonitor,
  ImageGenerated,
  VoiceIdStatus,
  VoiceTierStatus,
  VoiceModeStatus,
  answerAnnotationIsEmpty,
  applyLocalToolsEngaged,
  applyLocalToolsExecuted,
  applyLocalToolsOutOfSubset,
  applyLocalSub,
  applyLocalWarm,
  applyInferencePerf,
  applyModelSwap,
  applyModelTier,
  applySttTier,
  applyInterpretSegmentFed,
  applyInterpretSegment,
  applyTranscriptDiarized,
  applyUtteranceNoWake,
  applyVoiceTier,
  applyVoiceMode,
  applyVoiceIdEnrollProgress,
  applyVoiceIdEnrollStarted,
  applyVoiceIdEnrolled,
  applyVoiceIdForgot,
  applyVoiceIdVerify,
  bool,
  localToolsInitial,
  localWarmInitial,
  inferencePerfInitial,
  MODEL_LOCAL_WARM_EVENT,
  modelTierInitial,
  num,
  parseAnswerAnnotation,
  parseAuditSnapshot,
  parseCodeExplained,
  parseCodeProposed,
  parseShellBlocked,
  parseShellDenied,
  parseShellCommandEvent,
  parseShellRan,
  parseUiActuateBlocked,
  parseUiActuateRefused,
  parseUiActuateActionEvent,
  parseDraftComposed,
  parseMissionEvent,
  parseMacroRecorded,
  parseMacroReplayStep,
  parsePolicySnapshot,
  liveGateEventFrom,
  parseVerifyStatus,
  verifyStatusIsEmpty,
  parseCrossCheckStatus,
  crossCheckStatusIsEmpty,
  parseDebateStatus,
  debateStatusIsEmpty,
  parseDocIndexStatus,
  parseDocSearchResult,
  parseKnowledgeGraphResult,
  parseLifeLogDigest,
  parseLockdownStatus,
  parseNotebookActivity,
  parseUnifiedSearchResult,
  parseEpisodicRecorded,
  parseEvalReport,
  parseForgeProposed,
  parseCapabilityAtlas,
  parseTccSnapshot,
  parseTccAnomalies,
  TCC_ANOMALY_CAP,
  parseIntrospectSnapshot,
  introspectDriftLine,
  introspectAnomalyLine,
  introspectModuleViolationLine,
  introspectSecurityLine,
  parseIntrospectCapabilities,
  mergeIntrospectAlert,
  parseAttributionHealth,
  parseMcpStatus,
  parseWebhookEvent,
  applyWebhookEvent,
  webhookSurfaceInitial,
  parsePluginHandshake,
  applyPluginHandshake,
  parseMemoryRetention,
  parseOptimizerProposal,
  parseSecurityStatus,
  parseSkillsCatalog,
  parseSuggestion,
  parseProactiveDigest,
  parseFocusActive,
  parseUserModelConsolidated,
  parseVisionDescribe,
  parseAudioSoundMonitor,
  AUDIO_SOUND_MONITOR_EVENT,
  ScreenContext,
  screenContextInitial,
  applyScreenContextWatching,
  applyScreenContextConfigured,
  applyScreenContextCommand,
  SCREEN_CONTEXT_WATCHING_EVENT,
  SCREEN_CONTEXT_CONFIGURED_EVENT,
  SCREEN_CONTEXT_COMMAND_EVENT,
  parseImageGenerated,
  IMAGE_GENERATED_EVENT,
  ChartSpec,
  ReportReadout,
  parseChartSpec,
  parseReportReadout,
  str,
  strArr,
  sttTierInitial,
  audioIoInitial,
  voiceIdInitial,
  voiceTierInitial,
  voiceModeInitial,
} from "./events";
import { agentProfile, normalizeHue } from "./agents";

/* ------------------------------------------------------------------------ */

export type CoreState =
  | "offline"
  | "idle"
  | "listening"
  | "processing"
  | "thinking-local"
  | "thinking-cloud"
  | "speaking";

export interface TranscriptLine {
  who: "user" | "jarvis";
  text: string;
  ts: string; // envelope ts (verbatim from the daemon)
  routedTo?: string; // jarvis lines: "local" | "cloud" per route.completed
  seq: number;
}

export interface SystemGauges {
  cpuPercent: number | null;
  memUsedBytes: number | null;
  memTotalBytes: number | null;
  diskFreeBytes: number | null;
  uptimeSecs: number | null;
}

export interface PipelineTimings {
  sttMs: number;
  classifyMs: number;
  routeMs: number;
  speakMs: number;
  firstAudioMs: number | null;
  totalMs: number;
}

export interface IntentChip {
  intent: string;
  confidence: number;
  complexity: string;
}

export interface LearnedFact {
  key: string;
  value: string;
  ts: string;
  seq: number;
}

export interface ActionEntry {
  tool: string;
  outcome: string;
  ts: string;
  seq: number;
}

export type ToastKind = "learned" | "action" | "memory" | "info";

export interface Toast {
  id: number;
  kind: ToastKind;
  text: string;
  expiresAt: number; // ms epoch (action `at` clock)
  /** Marked on expiry/eviction; removed TOAST_EXIT_MS later so the view can
   *  play a fade-out instead of a single-frame cut. */
  exiting: boolean;
}

export interface HealStatus {
  event: "heal.suppressed" | "heal.triggered";
  errorsLast60s: number;
}

/** One item in a micro-app feed surface (e.g. global-scan). Shape mirrors
 *  events.ts::GlobalScanItem but is re-declared here so the pure core has no
 *  dependency on a specific app — fields are narrowed defensively on ingest. */
export interface FeedItem {
  title: string;
  source: string;
  url: string;
  published: string;
  category: string;
  summary: string;
}

/** Render-ready state for one running micro-app panel, keyed by app name in
 *  `appFeeds`. Populated by app.data relays; existence in the map (plus the
 *  `running` flag) is what flips a panel out of its OFFLINE placeholder. */
export interface AppFeed {
  running: boolean;
  brief: string;
  items: FeedItem[];
  fetchedAt: string | null;
  /** Latest status line, when the app emits one (feeds_ok/feeds_failed). */
  feedsOk: number | null;
  feedsFailed: number | null;
  /** envelope `at` of the last app.data — drives "stale feed" affordances. */
  updatedAt: number;
  /** Latest raw payload PER relay topic, keyed by the manifest topic string
   *  (apps.rs::resolve_topic). The feed-shaped fields above stay populated for
   *  the global-scan "feed" topic; topic-specific panels (e.g. Silicon Canvas's
   *  canvas.render_ms / canvas.viewport / canvas.selection) read their slice
   *  here and narrow it themselves. Opaque to the reducer beyond storage. */
  topics: Record<string, Record<string, unknown>>;
}

export type HealAlertKind = "rejected" | "blocked" | "applied";

/** Persistent self-heal alert (warning-triangle banner). Reserved for the
 *  ERROR-language states — rejected/blocked/applied — which use --alert-red.
 *  A validated *proposal* is NOT an error and is surfaced separately via
 *  `healProposal` (warn-amber panel), per the self-heal v2 safety contract.
 *  Stays until the user acknowledges it or a newer heal event replaces it. */
export interface HealAlert {
  kind: HealAlertKind;
  ts: string; // envelope ts
  /** Staging unix timestamp from the event data (the <ts> the user passes to
   *  scripts/apply_heal.sh), when the event carries one. */
  refTs: number | null;
  files: string[];
  detail: string;
}

/** Self-heal v2: the live root-cause diagnosis emitted before drafting
 *  (heal.diagnosing). Transient — superseded by the proposal (or a rejection)
 *  it leads to. Drives the "DIAGNOSING…" affordance on the SELF-REPAIR panel. */
export interface HealDiagnosing {
  signature: string;
  files: string[];
  subsystem: string;
  ts: string; // envelope ts
}

/** Self-heal v2 pending proposal — the validated, review-scored patch awaiting
 *  human review via scripts/apply_heal.sh. This is the warn-amber "attention"
 *  state (NOT an error): display-and-guide ONLY. There is deliberately no
 *  one-click apply — surfacing the gated command is the whole design. Persists
 *  until acknowledged or replaced by a newer heal event. */
export interface HealProposal {
  /** Proposal/staging unix timestamp — the <ts> for scripts/apply_heal.sh. */
  refTs: number | null;
  files: string[];
  /** Always reflects the daemon's gate result; the panel only shows the
   *  affirmative path when this is true (validated proposals are the only
   *  ones the daemon emits as heal.proposal). */
  validated: boolean;
  /** Adversarial-review confidence 0..1, or null for an older daemon. */
  confidence: number | null;
  /** Subsystem/signature carried forward from the diagnosis when available
   *  (event-echoed, else the last heal.diagnosing for the same burst). */
  subsystem: string;
  signature: string;
  ts: string; // envelope ts of the heal.proposal
}

/** Persistent kind for a Self-Forge alert banner — the ERROR/attention-language
 *  states (rejected/blocked) that use the red banner, mirroring HealAlertKind.
 *  A validated PROPOSAL is NOT here — it is surfaced separately via
 *  `forgeProposal` (warn-amber review panel). NOTE: "blocked" with reason
 *  "disabled" is the shipped-OFF state; the reducer keeps it OFF the red banner
 *  (it is not an error) — see the forge.blocked arm. */
export type ForgeAlertKind = "rejected" | "blocked";

/** Persistent Self-Forge alert (red banner) for the rejected/blocked error
 *  states. Stays until acknowledged or replaced by a newer forge event.
 *  `detail` is a short human reason; NEVER carries a secret. */
export interface ForgeAlert {
  kind: ForgeAlertKind;
  ts: string; // envelope ts
  detail: string;
}

/** Self-Forge pending PROPOSAL — a validated, sandboxed micro-app awaiting
 *  human review via scripts/apply_forge.sh. The warn-amber "attention" state
 *  (NOT an error): display-and-guide ONLY. There is deliberately NO one-click
 *  apply/deploy — surfacing the gated MANUAL command is the whole design
 *  (mirrors HealProposal). Nothing is installed or running yet. Persists until
 *  acknowledged or replaced by a newer forge event. NEVER carries a secret —
 *  the app name + the <ts> for the apply command only. */
export interface ForgeProposal {
  /** The forged app's name (forge.proposed `name`). */
  name: string;
  /** Proposal/staging unix timestamp — the <ts> for scripts/apply_forge.sh. */
  ts: number;
  /** envelope ts of the forge.proposed event. */
  at: string;
}

/** The honest reason a code draft/tool did NOT produce a usable result — a
 *  NON-error attention note (NOT the red alert chrome; review-only, like the
 *  docsearch "nothing found" line). Two kinds:
 *   - "rejected" — the model's draft was not a usable/confined diff (non-diff
 *     prose, a '..'/absolute path escape, or oversize); nothing was proposed.
 *   - "blocked"  — the tool did not run (an abort stage). "disabled" (the
 *     shipped-OFF gate) is NOT surfaced here — it is the inert default, not a
 *     failure, so the reducer drops it (mirrors forge.blocked reason=disabled).
 *  `detail` is a short human reason; NEVER carries a secret. */
export interface CodeNote {
  kind: "rejected" | "blocked";
  detail: string;
  at: string; // envelope ts
}

/** The CODE INTELLIGENCE surface — the HUD's read-only / propose-only view of
 *  the code_explain (grounded + cited answers) and code_propose_diff (a
 *  reviewable diff written to the proposal store) tools over the user's OWN
 *  allowlisted codebase root (daemon/src/code.rs). Mirrors the docsearch (cited
 *  hits) + forge/heal (propose-only review, MANUAL apply command) postures.
 *
 *  HONESTY (the line this surface must hold):
 *   - explanations are GROUNDED + CITED in the REAL indexed code (file+offset+
 *     snippet) — an empty `explained.hits` is the honest "not indexed", shown
 *     not hidden; nothing is ever fabricated.
 *   - proposals are PROPOSE-ONLY: the diff lives in the proposal store and the
 *     user's tree is UNTOUCHED. There is NO one-click apply — the panel shows
 *     ONLY the MANUAL command (scripts/apply_code_diff.sh <ts>), reviewed +
 *     applied by the user via the confined script.
 *   - the model's code QUALITY (does the diff compile/work) is runtime/model-
 *     gated and is NOT claimed measured.
 *  SECRET-FREE: every field is something the persona already speaks/shows (the
 *  question, the real cited chunks, a <ts>, a count, a short reason). */
export interface CodeIntel {
  /** The last cited explanation (code.explained). An empty `hits` is the honest
   *  not-indexed reply. null until the first explain. */
  explained: CodeExplained | null;
  /** The pending reviewable diff proposal (code.proposed) — REVIEW-ONLY, the
   *  panel shows ONLY the manual apply command. null until the first proposal,
   *  cleared by a fresh explain-only turn? No — kept until replaced/dismissed. */
  proposal: CodeProposal | null;
  /** The last honest non-error note (rejected/blocked) — cleared when a fresh
   *  proposal lands. NOT the red alert chrome (review-only attention). */
  note: CodeNote | null;
}

/** The SANDBOXED SHELL surface (#43) — the HUD's read-only view of the
 *  HIGHEST-RISK feature (arbitrary code execution). It is fed ONLY by the
 *  shell.* events the daemon emits from shell_run_tool (anthropic.rs), and is
 *  deliberately self-contained: the gated status (OFF/LOCKED) is conveyed by the
 *  shell.blocked reason=disabled event, not a separate read of the master switch.
 *
 *  HONESTY (the line this surface must hold):
 *   - every command is CONSEQUENTIAL: it PARKS for the user's spoken confirm and
 *     NEVER auto-runs (a shell.preview is the parked DryRun; the run only follows
 *     a separate shell.executing after the full gate);
 *   - it runs sandboxed DENY-DEFAULT (no network, confined fs, no secrets);
 *   - a destructive/exfil command is REFUSED PRE-exec (shell.denied), never run;
 *   - it ships OFF by default (shell.blocked reason=disabled is the inert gate);
 *   - it NEVER shows a (fabricable) command output — only the honest exit code +
 *     timed-out / truncated flags from shell.ran.
 *  SECRET-FREE: the wire carries only the command text, an outcome, a short
 *  reason, and the run flags — never an output, token, or secret. */
export interface ShellSurface {
  /** The last shell.* outcome (blocked-off / blocked-exec-failed / denied /
   *  parked / executing / ran). null until the first shell event arrives — the
   *  feature ships OFF, so nothing lands until [shell].enabled and a command is
   *  attempted. */
  last: ShellOutcome | null;
}

/** The GATED UI AUTOMATION surface (#44, the CAPSTONE) — the HUD's read-only
 *  view of the SINGLE MOST DANGEROUS feature (physically actuating the UI). It is
 *  fed ONLY by the ui_actuate.* events the daemon emits from ui_actuate_tool
 *  (anthropic.rs), and is deliberately self-contained: the gated status (OFF/
 *  LOCKED) is conveyed by the ui_actuate.blocked reason=disabled event, not a
 *  separate read of the master switch.
 *
 *  HONESTY (the line this surface must hold):
 *   - EVERY actuation is CONSEQUENTIAL + PER-ACTION gated: it PARKS for the
 *     user's spoken confirm and NEVER auto-runs (a ui_actuate.preview is the
 *     parked DryRun; the act only follows a separate ui_actuate.actuating after
 *     the full gate). ONE confirm authorizes EXACTLY ONE actuation — a second
 *     re-parks; never batched, never autonomous;
 *   - it performs exactly ONE action (a single click / type / key);
 *   - a degenerate / off-screen instruction is REFUSED PRE-actuation
 *     (ui_actuate.refused), never parked, never acted;
 *   - it ships OFF by default (ui_actuate.blocked reason=disabled is the inert
 *     gate);
 *   - the actuation itself is DEVICE-gated (Accessibility TCC consent); when
 *     consent is absent the act is honestly blocked (reason=device_gated), never
 *     a fabricated success.
 *  SECRET-FREE: the wire carries only the action class, a faithful target
 *  description, an outcome, and a short reason — never typed text or a secret. */
export interface UiActuateSurface {
  /** The last ui_actuate.* outcome (blocked-off / blocked-device / refused /
   *  parked / actuating / actuated). null until the first event arrives — the
   *  feature ships OFF, so nothing lands until [ui_automation].enabled and an
   *  actuation is attempted. */
  last: UiActuateOutcome | null;
}

/** The agent currently handling the request (CONTRACT part C.1). Set from
 *  agent.active events; drives the constellation highlight, the per-agent core
 *  hue override, and the status-bar chip. `hue` is always a normalized integer
 *  0..360. Null when nothing is active (idle) -> core damps back to cyan. */
export interface ActiveAgent {
  name: string;
  role: string;
  hue: number;
}

/** One entry in the EPISODIC TIMELINE — a single completed turn's episode-store
 *  outcome, folded from an `episodic.recorded` event. This is an ACTIVITY entry,
 *  NOT the episode's content: the redacted utterance/summary stay LOCAL in the
 *  daemon and are recalled only by voice (episodic_recall). `recorded` is whether
 *  the turn became a durable episode (false = honestly gated out: transient
 *  screen-read, empty/abandoned turn, voice-id UNVERIFIED, or the store off);
 *  `agent` is the recall scope it is bound to. */
export interface EpisodeEntry {
  recorded: boolean;
  agent: string;
  ts: string; // envelope ts (verbatim from the daemon)
  seq: number;
}

/** The MEMORY surface — the HUD's honest, telemetry-fed view of the episodic
 *  store + the user model. It holds ONLY observed ACTIVITY (counts, timestamps,
 *  agents, the eviction proof) — never episode bodies or profile entries, which
 *  are LOCAL to the daemon and inspected by voice. This is the privacy line: the
 *  HUD never broadcasts what JARVIS remembers, it reports THAT it remembered,
 *  bounded and forgettable. */
export interface MemoryState {
  /** Newest-first ring of episode-store outcomes (the timeline). Bounded. */
  timeline: EpisodeEntry[];
  /** How many of the turns we have seen actually became durable episodes
   *  (recorded=true) vs were gated out — an honest "what is kept" ratio. */
  recordedCount: number;
  gatedCount: number;
  /** The user model's last consolidation: how many profile entries were written
   *  the last time the reflection pass folded episodes+facts into the bounded
   *  compounding profile. null until the first consolidation. The entries
   *  THEMSELVES are read by voice (user_model_query), never streamed here. */
  userModelEntries: number | null;
  /** envelope ts of the last user_model.consolidated, or null. */
  userModelConsolidatedAt: string | null;
  /** Set when the last consolidation pass FAILED (busy/locked DB) — an honest
   *  "profile may be stale" affordance. Cleared by the next successful pass. */
  userModelStale: boolean;
  /** The last retention pass's episode eviction count + when — the PROOF the
   *  store is bounded (evict-oldest), not "remembers everything". null until the
   *  first pass that touched episodes. */
  lastEvictedEpisodes: number | null;
  lastRetentionAt: string | null;
}

/** The ACTION surface (#25 auto-draft / #26 durable missions / #27 macros) —
 *  the HUD's read-only, SECRET-FREE view of the three OFF-default action
 *  features. Always present (seeded empty) so the panel can render the honest
 *  empty resting state every feature ships at. NOTHING here is a button that
 *  sends / runs / replays: the panel is review-only.
 *
 *  HONESTY (held verbatim in the panel copy):
 *   - drafts: a draft is a SUGGESTION the user reviews + sends — JARVIS NEVER
 *     auto-sends it (the draft module has no send path; a send is a separate
 *     gated action). Only the subject + a bounded preview ride this surface,
 *     never the full body, never a secret.
 *   - missions: a persisted mission LOADS PAUSED on restart (never auto-runs);
 *     a resumed mission RE-GATES each consequential step (no pre-approval).
 *   - macros: a macro stores ONLY intents/utterances (never a secret/token);
 *     a replay re-runs each command through the gate EACH time (no bypass). */
export interface ActionSurface {
  /** PENDING drafts (status=draft), newest-first + bounded. A draft.forgotten
   *  removes one by id. A draft is NEVER sent from here. */
  drafts: PendingDraft[];
  /** DURABLE missions, newest-touched-first + bounded. Keyed by id; a fresh
   *  lifecycle event for an existing id REPLACES it in place (status update). */
  missions: DurableMission[];
  /** Recorded MACROS, newest-touched-first + bounded. Keyed by name; macro.recorded
   *  upserts, macro.forgotten removes, the replay lifecycle updates in place. */
  macros: MacroEntry[];
}

export interface HudState {
  connected: boolean;
  coreState: CoreState;
  /** `at` (ms) of the last coreState change OR last in-band evidence refresh
   *  — drives the 12s decay to idle. */
  stateSince: number;

  transcript: TranscriptLine[]; // ring buffer, newest last
  gauges: SystemGauges;
  lastTimings: PipelineTimings | null;

  /** Daemon-side is_speaking(): mic is muted because JARVIS is talking.
   *  Changes only at TTS boundaries — NOT per audio frame. */
  micMuted: boolean;
  /** Consecutive frames above LISTEN_ENTER_RMS while in `idle`. */
  loudStreak: number;
  /** Consecutive frames below LISTEN_EXIT_RMS while in `listening`. */
  quietStreak: number;

  facts: LearnedFact[]; // learned-facts ticker, newest first
  actions: ActionEntry[]; // actions ticker, newest first
  toasts: Toast[];

  lastIntent: IntentChip | null;
  cloudModel: string | null; // model id from the last route.cloud
  cloudKeyPresent: boolean | null; // daemon.started data (contract #2)
  daemonRoot: string | null;
  inferenceOffline: boolean; // sticky banner; cleared only by proof events
  heal: HealStatus | null;
  /** Red transient banner for heal rejected/blocked/applied (errors only). */
  healAlert: HealAlert | null;
  /** Live root-cause diagnosis (warn-amber), cleared when it resolves. */
  healDiagnosing: HealDiagnosing | null;
  /** Pending validated proposal awaiting gated human review (warn-amber). */
  healProposal: HealProposal | null;
  /** Pending validated FORGE proposal awaiting gated human review (warn-amber).
   *  Review-only: there is no auto-apply — the panel shows the manual command. */
  forgeProposal: ForgeProposal | null;
  /** Red transient banner for forge rejected/blocked errors (NOT the OFF state). */
  forgeAlert: ForgeAlert | null;
  /** The CODE INTELLIGENCE surface (code.explained / code.proposed / code.rejected
   *  / code.blocked): the last cited explanation + a pending PROPOSE-ONLY diff
   *  shown READ-ONLY with the MANUAL apply command. Null until the first code
   *  event; the feature ships OFF so nothing arrives until [code] is enabled +
   *  a codebase root allowlisted. REVIEW-ONLY + SECRET-FREE — no one-click apply,
   *  no token field on the wire. */
  codeIntel: CodeIntel | null;
  /** The SANDBOXED SHELL surface (shell.blocked / shell.denied / shell.preview /
   *  shell.executing / shell.ran): the last command's HONEST outcome — OFF/locked,
   *  refused-denylisted, parked-awaiting-confirm, executing, or the faithful run
   *  result (exit code + flags, NEVER an output). Null until the first shell
   *  event; the feature ships OFF so nothing arrives until [shell].enabled. Every
   *  command is consequential (parks for a spoken confirm, never auto-runs).
   *  READ-ONLY + SECRET-FREE — no output/token field on the wire. */
  shell: ShellSurface | null;
  /** The GATED UI AUTOMATION surface (#44, the CAPSTONE — ui_actuate.blocked /
   *  ui_actuate.refused / ui_actuate.preview / ui_actuate.actuating /
   *  ui_actuate.actuated): the last actuation's HONEST outcome — OFF/locked,
   *  refused-degenerate/off-screen, parked-awaiting-confirm, actuating, or the
   *  faithful single-action result. Null until the first ui_actuate event; the
   *  feature ships OFF so nothing arrives until [ui_automation].enabled. EVERY
   *  actuation is consequential + PER-ACTION gated (parks for a spoken confirm,
   *  never auto-runs; one confirm = one actuation; never batched/autonomous); the
   *  actuation itself is device-gated (Accessibility TCC). READ-ONLY + SECRET-FREE
   *  — no typed text/coordinate/token field on the wire. */
  uiActuate: UiActuateSurface | null;
  /** The MCP external-tool surface (mcp.status): configured servers, their
   *  connection status, exposed tools, and per-server agent allowlists. Null
   *  until the daemon emits the startup snapshot; the shipped-OFF default is a
   *  present-but-disabled snapshot (enabled=false, servers=[]). REVIEW-ONLY and
   *  SECRET-FREE — there is no token field to render. */
  mcp: McpStatus | null;
  /** The capability surface (capability.atlas): the master switch, armed/total
   *  counts, and every capability tagged armed/inert. Null until the daemon emits
   *  the startup snapshot. REVIEW-ONLY and SECRET-FREE. */
  capabilityAtlas: CapabilityAtlas | null;
  /** The ambient macOS app-privacy (TCC) status (tcc.snapshot): availability +
   *  grant count + count of HIGH-RISK grants currently allowed. Null until the
   *  sentinel emits its first scan. REVIEW-ONLY and SECRET-FREE. */
  tccSentinel: TccSentinel | null;
  /** Accumulated TCC anomaly alerts (tcc.anomaly): new grants / denied→allowed
   *  escalations, newest-first, deduped + capped. REVIEW-ONLY. */
  tccAnomalies: string[];
  /** The micro-app introspection tally (introspect.snapshot): sandboxed apps
   *  observed + profile-drift + resource-anomaly counts. Null until the sentinel
   *  emits its first tick. REVIEW-ONLY and SECRET-FREE. */
  introspect: IntrospectStatus | null;
  /** Accumulated introspection findings (introspect.profile_drift / .anomaly /
   *  .module_violation / .security_event) as human lines, newest-first, deduped +
   *  capped. REVIEW-ONLY — the sentinel reports, it never acts. */
  introspectAlerts: string[];
  /** Per-app DECLARED capability inventory (introspect.capabilities): the static
   *  "what can each app do" audit from manifests. Secret-free. REVIEW-ONLY. */
  introspectCapabilities: IntrospectCapability[];
  /** The ambient capability-health snapshot (attribution.health): how many of
   *  JARVIS's own agents/skills are reliable vs failing, with the failing ones
   *  flagged. Null until the sentinel emits. PROPOSE-ONLY (flags, never acts). */
  attributionHealth: AttributionHealth | null;
  /** The AT-REST ENCRYPTION surface (security.status): the honest posture of the
   *  opt-in, ships-OFF whole-file SQLCipher encryption — the [security].encrypt_memory
   *  config intent, the GROUND-TRUTH `active` flag (the master key actually resolved
   *  this run), the exact encrypted-vs-not scope arrays, and the verbatim
   *  honesty/key-location/cipher copy. Null until the daemon emits the startup
   *  snapshot. SECRET-FREE — there is NO key field on the wire; the indicator reads
   *  ENCRYPTED AT REST / NOT ENCRYPTED from `active` (never config alone), so a
   *  config-on-but-key-failed session reads honestly as NOT ENCRYPTED. Encryption
   *  protects AT REST ON DISK only — the in-RAM working set + key are NOT protected
   *  while jarvisd runs; the four sensitive SQLite stores + the voiceid owner blob
   *  are covered, the config TOML + Keychain item are not. */
  security: SecurityStatus | null;
  /** The WEBHOOK TRIGGERS surface (#35; webhook.received). The accumulated
   *  events-received count + the last secret-free decision {outcome,event,intent}.
   *  Always present (starts at received=0, last=null); an event arriving at all
   *  means the loopback listener is bound this session. SECRET-FREE + REVIEW-ONLY
   *  — never the body/secret/signature, and a webhook never auto-runs a
   *  consequential action (a consequential mapping reads as `parked`). */
  webhooks: WebhookSurface;
  /** The PLUGIN SDK surface (#36; plugin.handshake). The latest register-on-launch
   *  handshake per capability module — admitted (validated manifest, SBPL-sandboxed,
   *  scoped intents) or rejected (invalid_manifest / unauthorized). Null until the
   *  first handshake (the SDK ships OFF). SECRET-FREE — never the capability token. */
  plugins: PluginSurface | null;
  /** The PANIC / LOCKDOWN emergency-stop posture (lockdown.status): the
   *  process-global stop the daemon forces over every consequential / outward /
   *  autonomy / mic surface. Null until the daemon emits the startup snapshot;
   *  the shipped-OFF default snapshot is {locked:false, restoredFromMarker:false}.
   *  `locked` drives the LOCKED DOWN / NORMAL indicator + the prominent PANIC /
   *  UNLOCK controls; `restoredFromMarker` is true only when a restart re-entered
   *  lockdown from the persisted marker (so the user knows the stop survived a
   *  reboot). SECRET-FREE — booleans only. A panic/unlock COMMAND REPLY also
   *  carries `locked`, which the reducer/App folds in so the indicator flips
   *  immediately on a button press, ahead of the next telemetry frame. */
  lockdown: LockdownStatus | null;
  /** The skills marketplace catalog (skills.catalog): the hand-written in-tree
   *  skill library the Skills panel browses by category, with per-category counts,
   *  the real shipped total, and the live [skills] master-switch state. Null until
   *  the daemon emits the startup snapshot. REVIEW-ONLY and SECRET-FREE — a pure
   *  skill carries nothing secret and the snapshot is bounded to the discovery
   *  surface (name, category, description, consequential/source-gated markers). */
  skills: SkillsCatalog | null;
  /** The AGGREGATE-ONLY EVAL / OPTIMIZER scorecard (eval.report): measured
   *  latency p50/p95, rolling token sums + a labelled dollar ESTIMATE, routing
   *  accuracy + correction rate, and the honest optimizer posture (OFF / mode +
   *  PROPOSE-ONLY). Null until the daemon emits the first periodic report
   *  (~20s after startup). REVIEW-ONLY and PII-FREE — the wire carries only
   *  percentiles, sums, rates, and counts; latency/cost read "awaiting turns"
   *  until real turns/cloud calls feed them (runtime-gated). */
  evalReport: EvalReport | null;
  /** The last optimizer PROPOSAL (optimize.proposed): a REVIEWABLE artifact the
   *  propose-only optimizer wrote under state/optimize/proposals/<ts>/, awaiting
   *  the MANUAL scripts/apply_optimization.sh step. Null when none is pending —
   *  cleared by an optimize.none / optimize.suppressed round (nothing beat the
   *  baseline, or the master switch is off). REVIEW-ONLY: surfacing the gated
   *  command is the whole design; there is no one-click apply. SECRET-FREE. */
  optimizerProposal: OptimizerProposal | null;
  /** The on-device file-RAG index status (docsearch.indexed): how many files +
   *  chunks are indexed and how many carry an on-device vector. Null until the
   *  user runs an index (the feature ships OFF and indexes nothing until enabled
   *  AND a root is allowlisted), so a present value means a real index exists.
   *  COUNTS ONLY — no path or chunk text. `embeddedChunks` vs `chunks` tells the
   *  panel whether search runs neural or falls back to BM25. */
  docIndex: DocIndexStatus | null;
  /** The last CITED on-device file-search result (docsearch.searched): the query,
   *  the cited hits (real indexed file path + offset + bounded snippet + score),
   *  and the method that ACTUALLY ran. Null until the user searches their files.
   *  Hits are only ever real returned citations — never fabricated. */
  docSearch: DocSearchResult | null;
  /** The last UNIFIED-SEARCH result (unified.searched): one query fanned out
   *  across every AVAILABLE source, merged into ONE ranked list where each hit is
   *  ATTRIBUTED to its source + carries a real CITATION, plus the HONEST coverage
   *  (which sources were searched vs skipped, each skip with a reason). Null until
   *  the user runs a "search everything" query. Hits are only ever real returned
   *  citations — never fabricated; an empty hits[] with a non-empty searched set
   *  is the honest "searched X, found nothing". On-device source content never
   *  leaves the device; cloud sources appear here ONLY when connected. */
  unifiedSearch: UnifiedSearchResult | null;
  /** The last KNOWLEDGE-GRAPH build result (knowledge_graph.built): the build
   *  stats (chunks scanned / entities + relationships written / skipped at the
   *  bound), the honest extractor method token, and the resulting bounded SHARED
   *  world-model snapshot — entities (type/id/name + their `source` provenance
   *  citation) grouped by type, and relationships (from/relation/to + the
   *  `source file:offset` detail on the co-occurrence edge). Null until the user
   *  builds a graph (double-gated: [docsearch].enabled AND [docsearch].build_graph,
   *  both ship false). Every node/edge is EXTRACTED from real document text and
   *  provenance-tagged — never fabricated; the shipped extractor is a conservative
   *  heuristic (errs toward missing, not a sophisticated NER). Counts/ids/names/
   *  source strings only — no chunk text; rides the local broadcast only. */
  knowledgeGraph: KnowledgeGraphResult | null;
  /** The last ANSWER ANNOTATION (answer.annotated) — the HONEST provenance of the
   *  most recent answer: the REAL tool-result sources that informed it (each a
   *  real tool name + locator + bounded snippet), the honest "from my own
   *  knowledge" label when the turn used NO retrieval, and the model's
   *  self-reported confidence (grounded/inferred/uncertain + a one-line why). Null
   *  until the first answer.annotated arrives. The [answers] gates SHIP OFF, so
   *  until they are enabled every annotation is the empty (renders-nothing) shape:
   *  no sources, no from-my-knowledge label, no confidence. SECRET-FREE — citations
   *  are the real sources the persona already shows; "from my own knowledge" means
   *  no retrieval ran; confidence is the model's self-report, NOT a measured score.
   *  Never carries an embedding/audio/secret. */
  answerAnnotation: AnswerAnnotation | null;
  /** The last SELF-VERIFICATION outcome (answer.verified) — the per-turn result
   *  of the OPTIONAL second self-check pass ([answers].verify, which SHIPS OFF).
   *  When the gate is on AND the turn was important enough to gate in, the model
   *  critiques its own DRAFT answer ONCE against the real sources that turn used,
   *  and (at most ONCE) revises it. Carries only: the gate flag, the per-turn
   *  outcome token (off | verified-clean | revised | flagged), the derived badge
   *  (null => render nothing), and honest copy. Null until the first
   *  answer.verified arrives. With [answers].verify OFF (the shipped default) the
   *  outcome is "off" + null badge, so the panel renders nothing. HONEST: a second
   *  self-check REDUCES — does NOT eliminate — errors; VERIFIED does NOT mean
   *  guaranteed-correct. SECRET-FREE — never the flagged-claim text, never content
   *  beyond the answer, never an embedding/audio/secret. */
  verifyStatus: VerifyStatus | null;
  /** The last TOOL-RESULT CROSS-CHECK outcome (#21, answer.cross_checked) — the
   *  per-turn result of the BOUNDED plausibility cross-check of a tool result
   *  before it is surfaced as fact ([answers].cross_check, which SHIPS OFF). When
   *  the gate is on, deterministic sanity checks run (shape/range/contradiction/
   *  empty-vs-claimed/citation), plus an OPTIONAL single bounded model pass for
   *  important results. A failed check DOWNGRADES confidence + FLAGS the result —
   *  it NEVER removes a consequential action's confirmation gate. Carries only:
   *  the gate flag, the per-turn outcome token (off | plausible | flagged), the
   *  derived badge (null => render nothing), and honest copy. Null until the first
   *  answer.cross_checked arrives. With [answers].cross_check OFF (the shipped
   *  default) the outcome is "off" + null badge, so the panel renders nothing.
   *  HONEST: CHECKED means the checks found nothing to flag, NOT guaranteed-correct;
   *  UNVERIFIED means a check tripped + confidence was downgraded. SECRET-FREE —
   *  never the raw tool result, never the flag-reason text, never an
   *  embedding/audio/secret. */
  crossCheckStatus: CrossCheckStatus | null;
  /** The last MULTI-MODEL DEBATE outcome (#22, answer.debated) — the per-turn
   *  result of consulting TWO brains on a GATED high-stakes ask ([answers].debate,
   *  which SHIPS OFF; a conservative should_debate predicate means ordinary turns
   *  never debate). Bounded to at most two model calls. Carries only: the gate
   *  flag, the per-turn outcome token (off | agree | disagree | fallback), the
   *  derived badge (null => render nothing), and honest copy. Null until the first
   *  answer.debated arrives. With [answers].debate OFF (the shipped default), and
   *  on every ordinary turn, the outcome is "off" + null badge, so the panel
   *  renders nothing. HONEST: agreement RAISES confidence; disagreement SURFACES
   *  BOTH answers (never silently picked or averaged into a fake consensus); an
   *  unavailable second brain FALLS BACK to one and says so (runtime-gated, no
   *  fabricated agreement). SECRET-FREE — never the raw answers, never an
   *  embedding/audio/secret. */
  debateStatus: DebateStatus | null;
  /** The most-recent RESEARCH NOTEBOOK activity (notebook.card): a notebook voice
   *  command ("save this research" / "show my research notebook on X" / "what
   *  have I researched" / "forget my research on X") just ran. Carries the verb
   *  plus an OPTIONAL card — the topic, a bounded already-redacted snippet of the
   *  surfaced run, the REAL fetched-source citations (run-local id + title + url),
   *  and the saved-run count. Null until the first notebook command. PERSIST/READ
   *  ONLY: the daemon saves a real SAGE run that ALREADY happened and reads runs
   *  that were really saved — never a live fetch, never a fabricated source.
   *  save_none/forget_none/error carry NO card, so a no-op leaves the prior card
   *  in place (nothing new to surface). SECRET-FREE — only the verb, topic,
   *  bounded snippet, run count, and real citation locators; never raw content. */
  notebook: NotebookActivity | null;
  /** The most-recent LIFE-LOG DIGEST (lifelog.digest): a life-log voice command
   *  ("what did I do today/this week" / "show my life log") just ran. Carries the
   *  period, the honest-empty flag, the REAL recorded-episode count, the rendered
   *  digest text, and the bounded already-redacted themes / topics / recent
   *  summaries. Null until the first life-log command. READ-ONLY: it SUMMARIZES
   *  the user's real recorded (already-redacted) episodes — an empty window rides
   *  empty:true (the honest "nothing logged"), never a fabricated event. SECRET-
   *  FREE — every field is the episodic store's already-redacted, bounded output. */
  lifelog: LifeLogDigest | null;
  /** The CONSEQUENTIAL-GATE AUDIT surface (audit.snapshot): the daemon's
   *  hash-chained, tamper-EVIDENT log of every consequential decision — recent
   *  entries (newest-first, SECRET-FREE: agent/tool/REDACTED target/decision/
   *  outcome — never the raw input), the chain-OK verdict (or where it broke),
   *  the bounded total, and whether a prune re-rooted the chain. Null until the
   *  daemon emits the first snapshot; the shipped default is a present-but-empty
   *  snapshot (audit on by default — it is read-only accountability — but no
   *  consequential action has happened yet). REVIEW-ONLY. HONEST: tamper-EVIDENT
   *  (a careless edit is detected) is NOT tamper-PROOF (a root attacker who
   *  rewrites the whole on-disk chain still verifies). */
  audit: AuditSnapshot | null;
  /** The LIVE consequential-gate event ring (folded from the chokepoint
   *  telemetry policy.blocked / policy.auto_approved / confirm.parked), newest-
   *  first + bounded. The immediate-reaction surface BETWEEN authoritative
   *  audit.snapshot frames so the panel does not wait for the next poll. SECRET-
   *  FREE: the chokepoint events carry only tool/agent + an mcp/via marker. */
  liveGate: LiveGateEvent[];
  /** The user-set POLICY surface (policy.snapshot): the per-action rules
   *  (tool [+agent] [+recipient] -> always|never|ask) in the daemon's
   *  deterministic order, plus the [policy] on/off posture. Null until the daemon
   *  emits the first snapshot; the SHIPPED default is a present-but-empty snapshot
   *  (rules=[] => ASK everywhere => behavior is exactly today's gate). USER-SET
   *  ONLY: the editor writes via the command channel (an explicit user action),
   *  never by mutating this read-only snapshot, and there is NO agent/model path
   *  that can set a rule. HONEST: ALWAYS is a deliberate, master-gated loosening
   *  (inert when the master switch is OFF); NEVER always wins. */
  policy: PolicySnapshot | null;
  /** The on-device voice-id (speaker verification) surface, folded from the
   *  secret-free `voiceid.verify` event + the enrollment lifecycle
   *  (voiceid.enroll_started/_progress/enrolled/forgot). Always present (seeded
   *  with the honest OFF/not-enrolled resting state) so the indicator can render
   *  immediately. NEVER carries the embedding or any audio — only the on/enrolled
   *  flags, this turn's verified/UNRECOGNIZED verdict, and a SIMILARITY score (a
   *  similarity in [0,1], NOT a security guarantee). Voice-id RAISES the bar; it
   *  is an ADDED layer on top of the consequential gate, not a biometric. */
  voiceId: VoiceIdStatus;
  /** The live MODEL-TIER surface — folded from the per-turn `model.tier` verdict
   *  (which model answered: LOCAL/FAST/HEAVY + why: override/auto/fallback) and the
   *  most recent `model.swap` (a model-control voice command pinning a tier or
   *  clearing to AUTO). Always present (seeded with the honest AUTO/awaiting resting
   *  state) so the indicator renders immediately. MODEL-ONLY: a swap changes which
   *  model answers and NO safety gate. HONEST: LOCAL = on-device/private but
   *  capability-limited (NOT Opus-grade); AUTO is a per-turn difficulty heuristic;
   *  FALLBACK is the honest cloud-unreachable degrade to on-device. */
  modelTier: ModelTierStatus;

  /** The live RESIDENT-MODELS surface — folded from the config-derived
   *  `model.local_warm` startup snapshot (which local models the policy keeps warm
   *  under the RAM budget + whether multi-resident is in effect) plus the per-turn
   *  `model.tier`'s optional `local_sub` (the ACTIVE warm local model this turn).
   *  Always present (seeded with the honest single-resident/awaiting resting state)
   *  so the indicator renders immediately. HONEST: multi-resident keeps >1 local
   *  model warm for an INSTANT local swap ONLY when RAM allows (~2x RAM); single-
   *  resident is the safe low-RAM default; this is the PLAN, NOT a measured speed
   *  benefit (the swap benefit is device/RAM-dependent and not measured). It changes
   *  NO safety gate and does NOT change which tier is chosen. */
  localWarm: LocalWarmStatus;

  /** The live INFERENCE-PERF surface — folded from the per-turn `model.tier`
   *  payload's optional inference facts (speculative #37, quant #39, throttle #38).
   *  Always present (seeded with the honest awaiting/no-throttle resting state) so
   *  the panel renders immediately. READ-ONLY: it reports only the PATH THAT
   *  ACTUALLY RAN this turn — whether speculative decoding ran, the quant that
   *  actually loaded, and the active throttle plan (or none). HONEST: the real
   *  speedup / RAM-quality tradeoff / thermal-battery effect are device/model-gated
   *  and are NEVER measured or claimed here; all three ship OFF/neutral (off =>
   *  today's runtime); the live power read is device-gated ([power].adaptive). */
  inferencePerf: InferencePerfStatus;

  /** The live OFFLINE TOOL-LOOP surface — folded from the per-turn
   *  `local_tools.engaged` verdict (a safe local tool actually ran offline: the
   *  ACTING OFFLINE signal) plus the per-tool `local_tools.executed` /
   *  `local_tools.out_of_subset` activity. Always present (seeded with the honest
   *  "chatting" resting state) so the indicator renders immediately. ACTIVITY-ONLY:
   *  it shows WHAT the on-device path did (used local tools vs chatted), it changes
   *  NO gate. HONEST: the on-device ~4B is LESS RELIABLE at tool-calling than the
   *  cloud model (bounded + falls back); the SAME safety gates (confirmation,
   *  voice-id, lockdown, policy) apply offline — `gated` surfaces when one fired,
   *  `refusedOutOfSubset` when the 4B reached outside the safe subset and was
   *  stopped. The model.tier telemetry already marks the Local tier this turn. */
  localTools: LocalToolsStatus;

  /** The live VOICE-TIER surface — folded from the per-reply `voice.tier`
   *  telemetry (which TTS backend voiced the last reply: ON-DEVICE Kokoro vs the
   *  optional CLOUD ElevenLabs voices). Always present (seeded with the honest
   *  awaiting resting state). VOICE-ONLY: it changes how JARVIS SOUNDS, no safety
   *  gate. HONEST: ON-DEVICE = private/offline default + fallback; CLOUD VOICE =
   *  premium voices where the spoken text leaves the device to synthesize. The
   *  telemetry carries NO key/voice id — only {backend, agent}. */
  voiceTier: VoiceTierStatus;

  /** The live STT-TIER surface — folded from the per-turn `stt.tier` telemetry
   *  (which STT backend transcribed the last captured audio: ON-DEVICE whisper vs
   *  the optional gated CLOUD ElevenLabs Scribe). Always present (seeded with the
   *  honest awaiting resting state). HONEST: ON-DEVICE whisper is the private/offline
   *  default + the fallback on any cloud error; CLOUD STT means the user's VOICE
   *  AUDIO left the device to be transcribed — MORE sensitive than the TTS text leg.
   *  The telemetry carries NO key/transcript/audio — only {backend}. */
  sttTier: SttTierStatus;

  /** The read-only AUDIO-I/O surface (#30 live interpretation / #31 multi-speaker
   *  diarization / #32 custom wake-word). Always present (seeded with the honest
   *  OFF/neutral resting state all three features ship at) so the panel renders
   *  immediately. HONEST by construction: live interpretation + the always-listening
   *  loop are DEVICE-GATED (mic) — only `interpret.active` (a segment was fed) and the
   *  real-translation count surface, never a fabricated translation; diarization is
   *  ElevenLabs-Scribe-ONLY (`backendCanDiarize` is the ground-truth bit — false on
   *  on-device whisper, which is an honest single stream, never a fabricated speaker);
   *  the active wake word is the configured phrase (default "jarvis"). SECRET-FREE —
   *  only languages / booleans / counts / the wake phrase ride this surface; never the
   *  transcript text, a translation, or the wav path. */
  audioIo: AudioIoStatus;

  /** The live VOICE-MODE surface — folded from the per-reply `voice.prosody`
   *  telemetry (#33 adaptive tone + #34 whisper). Always present (seeded with the
   *  honest OFF/neutral default both features ship at). EXPRESSIVENESS-ONLY: it
   *  changes how JARVIS SOUNDS (tone + soft/terse delivery), no safety gate. HONEST:
   *  the `rich` bit is the ground truth that EL-v3 audio-tags/stability/style were
   *  ACTUALLY applied (true only on ElevenLabs v3 — Kokoro/non-v3 get a coarse
   *  rate-only mapping, never faked); whisper changes DELIVERY only and never
   *  suppresses a required confirmation. The telemetry carries NO key/voice id/text
   *  — only {profile, backend, rich, whisper, terse, rate, volume}. */
  voiceMode: VoiceModeStatus;
  lastError: { event: string; detail: string; ts: string } | null;

  /** Running micro-apps, by manifest name (app.started/app.stopped). */
  runningApps: ReadonlySet<string>;
  /** Per-app feed surfaces, keyed by manifest name (app.data relays). */
  appFeeds: Record<string, AppFeed>;

  /** The last ON-DEVICE VLM describe outcome (vision.describe, channel "local").
   *  METADATA ONLY — source kind + whether the on-device VLM actually produced a
   *  description (`available`) + whether the model is enabled (`vlm`). Carries NO
   *  pixels / NO description text / NO path: the visual content (the most
   *  sensitive thing in the describe op) NEVER rides telemetry, so the panel's
   *  VISUAL DESCRIPTION readout surfaces only this honest posture, never the
   *  scene. DISTINCT from the OCR vision.screen readout (OCR = text glyphs; VLM =
   *  visual understanding). Null until the daemon emits the first describe. */
  visionDescribe: VisionDescribe | null;

  /** The OPT-IN ambient sound-monitor STATE (audio.sound_monitor, channel
   *  "local"), or null until the daemon emits it at startup. Drives the HUD's
   *  MONITORING / OFF indicator. SHIPS OFF + pinned: `enabled` is the operator's
   *  config opt-in (no tool/agent/model can flip it, no auto-arm). Even when
   *  enabled, continuous ambient capture is DEVICE-GATED behind macOS mic/TCC
   *  consent (`consent: "device_gated"`). LABELS ONLY — the audio never leaves the
   *  device. DISTINCT from the one-shot vision.sound classify readout. */
  audioSoundMonitor: AudioSoundMonitor | null;

  /** The CONTINUOUS SCREEN-CONTEXT posture (#42) — folded from the three
   *  secret-free screen_context.* system envelopes (configured at startup,
   *  watching on each continuous snapshot, command on a recall/forget). Always
   *  present (seeded with the honest OFF-default resting state) so the WATCHING
   *  indicator can render immediately. SECRET-FREE by construction: it holds ONLY
   *  the loop-active bit (the PROMINENT amber WATCHING indicator), the BOUNDED
   *  ring counts (held N / cap M), the startup config bounds (enabled +
   *  interval), and the last command verb — NEVER the recognized glyphs or the
   *  recalled redacted text (those live ONLY in the daemon's transient in-RAM
   *  ring, never on this wire). HONEST: OFF by default; the live capture is
   *  TCC-DEVICE-GATED (Screen Recording, not SBPL-grantable); the ring is
   *  TRANSIENT (off lifelong memory / optimizer), glyph-only (NEVER a face /
   *  person id / embedding; pixels never leave the device), BOUNDED (evict-
   *  oldest, held <= cap), FORGETTABLE ("forget my screen context" wipes it),
   *  and READ-ONLY (recall describes, never actuates). */
  screenContext: ScreenContext;

  /** The last ON-DEVICE IMAGE-GENERATION outcome (image.generated, channel
   *  "local"), or null until the daemon emits one. METADATA ONLY — whether the
   *  on-device MLX diffusion model actually produced an image (`available`),
   *  WHERE on the device the image landed (`path`, a local abs path under
   *  state/images/), and non-secret model/size/steps metadata + the
   *  `image` (cfg.image.enabled) opt-in flag. Carries NO prompt / NO pixels: the
   *  two most sensitive things in the op (what was asked + the image) NEVER ride
   *  telemetry, and the diffusion seed is intentionally dropped. The image is
   *  100% ON-DEVICE (MLX diffusion; the prompt + image never leave the machine —
   *  NO cloud); DEVICE-GATED on a multi-GB model + RAM, so it SHIPS OFF/opt-in.
   *  `available` is false on every gate/unavailable — NEVER a fabricated image,
   *  NEVER a silent cloud fall-back. */
  imageGenerated: ImageGenerated | null;

  /** The agent currently handling the request, or null when idle (CONTRACT
   *  part C). The constellation panel always shows the full static roster;
   *  this only tracks WHO is lit. */
  activeAgent: ActiveAgent | null;

  /** The MEMORY surface — the episodic-store + user-model ACTIVITY view (Core-A
   *  / Core-B), fed by episodic.recorded / user_model.consolidated[_failed] /
   *  memory.retention telemetry. Holds counts/timestamps/agents + the eviction
   *  proof only; episode bodies + profile entries stay LOCAL (voice-inspected). */
  memory: MemoryState;

  /** The PROACTIVE-INTELLIGENCE SUGGESTIONS feed (#13 + #14) — the propose-only
   *  habit-automation offers + predictive suggestions the daemon mined from the
   *  redacted, agent-scoped episodic store (proactive.suggestion). Newest-first +
   *  bounded. These are OBSERVED-pattern SUGGESTIONS, never actions: JARVIS never
   *  auto-acts on them (every card carries auto_acts=false). A habit offer's
   *  Accept routes through the EXISTING gated standing-mission creation verb (NOT
   *  an ungated create); a predictive suggestion is intel only (no Accept). Ships
   *  OFF (mirrors proactive.speak): with [proactive] off the daemon emits no
   *  cards, so this stays empty. A card whose id is in `dismissedSuggestions` is
   *  suppressed (the re-offer dedup). */
  suggestions: Suggestion[];

  /** The DISMISS LEDGER — ids the user has dismissed. A dismissed suggestion is
   *  dropped from `suggestions` AND its id is recorded here so a later
   *  re-emission of the SAME id (the daemon re-mines the same recurring pattern)
   *  is suppressed rather than re-offered. Bounded so it cannot grow without
   *  limit. */
  dismissedSuggestions: ReadonlySet<string>;

  /** The SMARTER BRIEF digest (#23, proactive.digest) — the daemon's PURE
   *  ranked/capped/cited brief built from the verified, injected signal snapshot.
   *  Each item carries its REAL source citation (calendar event id / message id /
   *  news source); an unconnected source contributes nothing (honestly absent,
   *  never padded). Null until the first proactive.digest arrives. The daemon
   *  only emits a NON-empty digest, so when present this holds at least one cited
   *  item — but the panel still renders the honest-empty copy if it ever receives
   *  an empty one (defensive). SECRET-FREE: only a priority + an honest line + a
   *  rendered citation per row. */
  proactiveDigest: ProactiveDigest | null;

  /** The active FOCUS PROFILE posture (#24, focus.active) — which signal
   *  categories surface, the brief verbosity, whether suggestions are quieted,
   *  plus the PERMISSION-NEUTRAL contract (read from the wire, pinned to the only
   *  honest values). A focus profile only ever QUIETS/FOCUSES what surfaces — it
   *  NEVER loosens a gate, enables an action, or raises autonomy. Null until the
   *  daemon emits the startup focus.active card; the shipped default
   *  ([focus].profile = "default") is the IDENTITY (today's behavior — nothing
   *  quieted). SECRET-FREE: only the profile name + category labels + the posture
   *  booleans. */
  focusProfile: FocusActive | null;

  /** The ACTION surface (#25 auto-draft / #26 durable missions / #27 macros):
   *  pending drafts (subject/preview only — NEVER the full body), durable
   *  missions (id/goal/status + sub-task progress), and recorded macros (name +
   *  step count + last replay outcome). Always present (seeded empty); all three
   *  features SHIP OFF, so nothing arrives until the operator enables a flag.
   *  REVIEW-ONLY + SECRET-FREE — no send/run/replay button, no body/token field
   *  on the wire to render. */
  actionSurface: ActionSurface;

  /** The last DATA->CHART spec (#41, chart.data) — the exact series the daemon's
   *  chart.rs emitted for the Chart component to render verbatim (every emitted
   *  point plotted, line segments only between GIVEN points, NO interpolation/
   *  invented/extrapolated point, axis ranges derived from the data). Null until
   *  the first chart.data; the op ships OFF ([chart].enabled) so nothing arrives
   *  until it is enabled AND a "chart this" command runs. An honest-empty spec
   *  (no plottable point) rides `empty: true` so the panel shows the honest-empty
   *  state rather than a fabricated point. SECRET-FREE — labels/axes/title/points
   *  only. */
  chart: ChartSpec | null;

  /** The last REPORT READOUT (#40, report.built) — the HUD's honest view of the
   *  report the daemon's report.rs assembled from already-cited notebook/research
   *  sources: the title, the section + citation counts, the section headings, and
   *  the REAL citations (every one a source ref an input claim carried — never
   *  fabricated). Null until the first report.built that carries a report (the
   *  off/error verbs carry no report, so the panel shows nothing); the op ships
   *  OFF ([report].enabled). An honest-empty report (no citable source) rides
   *  `empty: true` so the panel says "no sources to report on" rather than a
   *  fabricated body. REVIEW-ONLY + SECRET-FREE — counts/headings/locators only. */
  report: ReportReadout | null;

  seq: number; // monotonic id source for lines/facts/actions/toasts
}

/* ------------------------------------------------------------------------ */

export const TRANSCRIPT_CAP = 100;
export const TICKER_CAP = 24;
/** Cap on the episodic TIMELINE ring the HUD keeps in view (newest-first). This
 *  is a VIEW bound, independent of the daemon's own bounded [episodic].retention
 *  cap on the durable store — the panel shows the most recent turns, not the
 *  whole local history (which lives in SQLite and is recalled by voice). */
export const EPISODE_TIMELINE_CAP = 24;
/** Defensive cap on a forwarded-op string before it reaches the ticker/toast.
 *  action.executed outcomes are capped daemon-side (120ch); app.op_forwarded
 *  `op` is NOT, so bound it here so a long/odd op line cannot overflow the
 *  activity surfaces (same hygiene as APP_FEED_ITEM_CAP). */
export const OP_FORWARD_OUTCOME_CAP = 120;
/** Cap on items kept per app feed surface. The app already sends ~top-20
 *  newest-first; we hard-cap so a misbehaving/compromised app cannot grow
 *  the panel slice without bound. */
export const APP_FEED_ITEM_CAP = 30;
/** Cap on the LIVE consequential-gate event ring the HUD keeps between
 *  authoritative audit.snapshot frames. A VIEW bound only — the durable,
 *  hash-chained record lives daemon-side (state/audit.db) and is the source of
 *  truth; this is just the immediate-reaction surface. */
export const LIVE_GATE_CAP = 24;
/** Cap on the live proactive-SUGGESTIONS feed the HUD keeps in view. A VIEW
 *  bound — the daemon's detector is itself bounded; this just keeps the panel
 *  from growing without limit if many distinct patterns clear the threshold. */
export const SUGGESTION_CAP = 12;
/** Cap on the dismiss ledger (ids the user dismissed, suppressed on re-offer).
 *  Bounded so a long session cannot grow the suppression set unboundedly;
 *  oldest dismissed ids fall off (the worst case is one stale id is re-offered
 *  once, which is harmless and itself re-dismissible). */
export const DISMISS_LEDGER_CAP = 64;
/** VIEW caps on the action surface (#25/#26/#27). Bounded so a long session or a
 *  misbehaving daemon frame cannot grow the read-only panel without limit; the
 *  durable records live in the daemon's SQLite store, this is just the view. */
export const DRAFT_CAP = 12;
export const MISSION_CAP = 12;
export const MACRO_CAP = 16;
export const TOAST_CAP = 5;
export const TOAST_TTL_MS = 4500;
/** Fade-out window between a toast expiring and its removal. */
export const TOAST_EXIT_MS = 240;

/** Hysteresis pair: enter listening strictly ABOVE 0.018, count quiet
 *  frames only strictly BELOW 0.012. The band between holds the current
 *  state, so rms hovering at the old 0.015 threshold cannot thrash. */
export const LISTEN_ENTER_RMS = 0.018;
export const LISTEN_EXIT_RMS = 0.012;
/** audio.level arrives every ~66ms; 2 consecutive loud frames ~= 130ms of
 *  sustained evidence before promoting idle -> listening. */
export const ENTER_FRAMES_TO_LISTEN = 2;
/** 9 quiet frames ~= 600ms of sustained silence before listening -> idle. */
export const QUIET_FRAMES_TO_IDLE = 9;
/** HUD.md §2.1: any transient state decays to idle after 12s without events
 *  (telemetry is fire-and-forget; terminal events can be missed). */
export const STALE_STATE_MS = 12_000;
/** In-band evidence (loud frames while listening, speaking=true frames while
 *  speaking) refreshes stateSince at most this often, so long dictation or a
 *  long TTS reply never hits the stale decay, without re-rendering per frame. */
export const EVIDENCE_REFRESH_MS = 4_000;

export type HudAction =
  | { type: "ws.connected"; at: number }
  | { type: "ws.disconnected"; at: number }
  | { type: "telemetry"; envelope: TelemetryEnvelope; at: number }
  | { type: "tick"; at: number }
  | { type: "alert.dismiss" }
  // Task #12 — fold the `locked` verdict from a panic/unlock COMMAND REPLY into
  // the indicator IMMEDIATELY on the button press (ahead of the next telemetry
  // frame). It preserves a prior restoredFromMarker on engage but clears it on a
  // user unlock (the stop no longer survives a restart). The authoritative source
  // remains lockdown.status telemetry; this is just the instant local echo.
  | { type: "lockdown.set"; locked: boolean }
  // #13/#14 — DISMISS a proactive suggestion by id. Drops it from the live feed
  // AND records the id in the dismiss ledger so the SAME id is suppressed on a
  // later re-emission (the re-offer dedup). A dismiss never accepts/acts — it is
  // the "no thanks" half of the propose-only contract. Accept is NOT a reducer
  // action: it sends the gated standing-mission creation command (App.tsx), then
  // dismisses the offer locally so it is not re-shown.
  | { type: "suggestion.dismiss"; id: string }
  // WS4 auto-update — push a single transient INFO toast for the silent
  // launch-update notice ("Updating JARVIS to <version>…"). This is a pure,
  // non-blocking surface action (no daemon, no authority): it only adds a toast
  // to the existing stack so an auto-install (pref ON) is never a silent
  // surprise. It does NOT download/install — the install runs through the
  // existing signed backend command in App; this is just the visible notice.
  | { type: "notice.toast"; text: string; at: number };

export function initialState(): HudState {
  return {
    connected: false,
    coreState: "offline",
    stateSince: 0,
    transcript: [],
    gauges: {
      cpuPercent: null,
      memUsedBytes: null,
      memTotalBytes: null,
      diskFreeBytes: null,
      uptimeSecs: null,
    },
    lastTimings: null,
    micMuted: false,
    loudStreak: 0,
    quietStreak: 0,
    facts: [],
    actions: [],
    toasts: [],
    lastIntent: null,
    cloudModel: null,
    cloudKeyPresent: null,
    daemonRoot: null,
    inferenceOffline: false,
    heal: null,
    healAlert: null,
    healDiagnosing: null,
    healProposal: null,
    forgeProposal: null,
    forgeAlert: null,
    codeIntel: null,
    shell: null,
    uiActuate: null,
    mcp: null,
    capabilityAtlas: null,
    tccSentinel: null,
    tccAnomalies: [],
    introspect: null,
    introspectAlerts: [],
    introspectCapabilities: [],
    attributionHealth: null,
    security: null,
    webhooks: webhookSurfaceInitial(),
    plugins: null,
    lockdown: null,
    skills: null,
    evalReport: null,
    optimizerProposal: null,
    docIndex: null,
    docSearch: null,
    unifiedSearch: null,
    knowledgeGraph: null,
    answerAnnotation: null,
    verifyStatus: null,
    crossCheckStatus: null,
    debateStatus: null,
    notebook: null,
    lifelog: null,
    audit: null,
    liveGate: [],
    policy: null,
    voiceId: voiceIdInitial(),
    modelTier: modelTierInitial(),
    localWarm: localWarmInitial(),
    inferencePerf: inferencePerfInitial(),
    localTools: localToolsInitial(),
    voiceTier: voiceTierInitial(),
    sttTier: sttTierInitial(),
    audioIo: audioIoInitial(),
    voiceMode: voiceModeInitial(),
    lastError: null,
    runningApps: new Set<string>(),
    appFeeds: {},
    visionDescribe: null,
    audioSoundMonitor: null,
    screenContext: screenContextInitial(),
    imageGenerated: null,
    activeAgent: null,
    memory: {
      timeline: [],
      recordedCount: 0,
      gatedCount: 0,
      userModelEntries: null,
      userModelConsolidatedAt: null,
      userModelStale: false,
      lastEvictedEpisodes: null,
      lastRetentionAt: null,
    },
    suggestions: [],
    dismissedSuggestions: new Set<string>(),
    proactiveDigest: null,
    focusProfile: null,
    actionSurface: { drafts: [], missions: [], macros: [] },
    chart: null,
    report: null,
    seq: 0,
  };
}

/* ------------------------------------------------------------------------ */

const TRANSIENT: ReadonlySet<CoreState> = new Set([
  "listening",
  "processing",
  "thinking-local",
  "thinking-cloud",
  "speaking",
]);

/** Events that PROVE the local inference server responded — the only things
 *  allowed to clear the LOCAL INFERENCE OFFLINE banner. (`opener.played` is
 *  source "local" but fires before STT ever contacts the server, which made
 *  the banner blink once per exchange while the server was down.) */
const INFERENCE_PROOF_EVENTS: ReadonlySet<string> = new Set([
  "stt.transcript",
  "stt.empty",
  "intent.classified",
  "memory.learned",
]);

function setCore(state: HudState, core: CoreState, at: number): HudState {
  if (state.coreState === core) return state;
  return { ...state, coreState: core, stateSince: at, loudStreak: 0, quietStreak: 0 };
}

function pushToast(state: HudState, kind: ToastKind, text: string, at: number): HudState {
  const id = state.seq + 1;
  let toasts: Toast[] = [
    ...state.toasts,
    { id, kind, text, expiresAt: at + TOAST_TTL_MS, exiting: false },
  ];
  // Cap by ACTIVE count: overflow marks the oldest active toast(s) exiting
  // (fade-out) instead of deleting them in a single frame.
  let overflow = toasts.filter((t) => !t.exiting).length - TOAST_CAP;
  if (overflow > 0) {
    toasts = toasts.map((t) => {
      if (overflow > 0 && !t.exiting) {
        overflow -= 1;
        return { ...t, exiting: true, expiresAt: at };
      }
      return t;
    });
  }
  return { ...state, seq: id, toasts };
}

function pushTranscript(state: HudState, line: Omit<TranscriptLine, "seq">): HudState {
  const seq = state.seq + 1;
  const transcript = [...state.transcript, { ...line, seq }];
  return { ...state, seq, transcript: transcript.slice(-TRANSCRIPT_CAP) };
}

/* micro-app feed helpers ---------------------------------------------------- */

function emptyAppFeed(running: boolean): AppFeed {
  return {
    running,
    brief: "",
    items: [],
    fetchedAt: null,
    feedsOk: null,
    feedsFailed: null,
    updatedAt: 0,
    topics: {},
  };
}

function isObj(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** Coerce one untrusted relayed item object into a FeedItem. Missing/wrongly
 *  typed fields collapse to "" — a malformed item never throws and never
 *  drags non-string junk into the render path. */
function coerceFeedItem(o: Record<string, unknown>): FeedItem {
  return {
    title: str(o, "title") ?? "",
    source: str(o, "source") ?? "",
    url: str(o, "url") ?? "",
    published: str(o, "published") ?? "",
    category: str(o, "category") ?? "",
    summary: str(o, "summary") ?? "",
  };
}

/* ------------------------------------------------------------------------ */

export function reduce(state: HudState, action: HudAction): HudState {
  switch (action.type) {
    case "ws.connected": {
      const next = { ...state, connected: true };
      // The core idles while we wait for the first event.
      return next.coreState === "offline" ? setCore(next, "idle", action.at) : next;
    }
    case "ws.disconnected": {
      // Idempotent: repeated failed reconnect attempts while already offline
      // must not churn state (full-tree re-render every backoff tick).
      if (!state.connected && state.coreState === "offline") return state;
      return setCore({ ...state, connected: false }, "offline", action.at);
    }
    case "tick": {
      let next = state;
      // Toast lifecycle: expired -> exiting (fade-out), exiting -> removed.
      const anyExpiring = next.toasts.some((t) => !t.exiting && t.expiresAt <= action.at);
      const anyRemovable = next.toasts.some(
        (t) => t.exiting && t.expiresAt + TOAST_EXIT_MS <= action.at,
      );
      if (anyExpiring || anyRemovable) {
        next = {
          ...next,
          toasts: next.toasts
            .filter((t) => !(t.exiting && t.expiresAt + TOAST_EXIT_MS <= action.at))
            .map((t) =>
              !t.exiting && t.expiresAt <= action.at ? { ...t, exiting: true } : t,
            ),
        };
      }
      // Stuck-state decay (missed terminal event). stateSince is refreshed by
      // in-band evidence, so actively-evidenced states never decay.
      if (
        TRANSIENT.has(next.coreState) &&
        action.at - next.stateSince >= STALE_STATE_MS
      ) {
        next = setCore(next, "idle", action.at);
        // A missed terminal event also stranded the active agent — release it
        // so the core damps back to cyan rather than holding the agent hue.
        if (next.activeAgent !== null) next = { ...next, activeAgent: null };
      }
      return next;
    }
    case "alert.dismiss": {
      // Acknowledge any standing heal OR forge surface — the red alert banners,
      // a live diagnosis, or a pending proposal. No-op (same reference) when all
      // clear so a stray ACK never churns the tree.
      if (
        state.healAlert === null &&
        state.healDiagnosing === null &&
        state.healProposal === null &&
        state.forgeProposal === null &&
        state.forgeAlert === null
      ) {
        return state;
      }
      return {
        ...state,
        healAlert: null,
        healDiagnosing: null,
        healProposal: null,
        forgeProposal: null,
        forgeAlert: null,
      };
    }
    case "telemetry":
      return applyEnvelope(state, action.envelope, action.at);
    case "lockdown.set": {
      // Instant local echo of a panic/unlock button reply's `locked` verdict so
      // the indicator flips on the press. On a user UNLOCK (locked=false) the
      // restoredFromMarker flag is cleared too (the stop no longer persists); on
      // an engage we keep any prior restored flag. lockdown.status telemetry, if
      // it arrives, remains authoritative and will overwrite this.
      const prev = state.lockdown;
      const next: LockdownStatus = {
        locked: action.locked,
        restoredFromMarker: action.locked ? (prev?.restoredFromMarker ?? false) : false,
      };
      if (prev !== null && prev.locked === next.locked &&
          prev.restoredFromMarker === next.restoredFromMarker) {
        return state; // no churn when nothing changed
      }
      return { ...state, lockdown: next };
    }
    case "suggestion.dismiss": {
      // Drop the suggestion from the live feed AND record its id so a later
      // re-emission of the same recurring-pattern id stays suppressed. Idempotent:
      // if the id is already gone AND already ledgered, return the same reference
      // (a stray dismiss never churns the tree). The ledger is bounded — oldest
      // dismissed ids fall off (insertion order), which at worst lets one stale id
      // be re-offered once (itself re-dismissible).
      const present = state.suggestions.some((x) => x.id === action.id);
      if (!present && state.dismissedSuggestions.has(action.id)) return state;
      const suggestions = present
        ? state.suggestions.filter((x) => x.id !== action.id)
        : state.suggestions;
      const ledger = new Set(state.dismissedSuggestions);
      ledger.add(action.id);
      while (ledger.size > DISMISS_LEDGER_CAP) {
        const oldest = ledger.values().next().value as string | undefined;
        if (oldest === undefined) break;
        ledger.delete(oldest);
      }
      return { ...state, suggestions, dismissedSuggestions: ledger };
    }
    case "notice.toast": {
      // A single transient INFO toast (the silent launch-update notice). Empty
      // text is a no-op so a missing version never churns the tree.
      if (!action.text) return state;
      return pushToast(state, "info", action.text, action.at);
    }
  }
}

function applyEnvelope(state: HudState, env: TelemetryEnvelope, at: number): HudState {
  // The inference banner clears only on events that prove the inference
  // server actually responded — never on merely source=="local" envelopes.
  let s = state;
  if (s.inferenceOffline && INFERENCE_PROOF_EVENTS.has(env.event)) {
    s = { ...s, inferenceOffline: false };
  }

  switch (env.event) {
    case "audio.level": {
      const rms = num(env.data, "rms");
      if (rms === null) return s;
      const speaking = bool(env.data, "speaking") ?? false;
      // Live rms/history go to core/audioStore.ts (refs, not React state) —
      // here only the state machine. Unchanged fields => same reference.
      let next = s;
      if (speaking !== next.micMuted) next = { ...next, micMuted: speaking };

      if (next.coreState === "idle") {
        if (rms > LISTEN_ENTER_RMS && !speaking) {
          const loudStreak = next.loudStreak + 1;
          next =
            loudStreak >= ENTER_FRAMES_TO_LISTEN
              ? setCore(next, "listening", at)
              : { ...next, loudStreak };
        } else if (next.loudStreak !== 0) {
          next = { ...next, loudStreak: 0 };
        }
      } else if (next.coreState === "listening") {
        if (rms > LISTEN_EXIT_RMS && !speaking) {
          // Still audible: hold listening. Loud frames also act as a
          // keepalive against the 12s stale decay during long dictation.
          const refresh =
            rms > LISTEN_ENTER_RMS && at - next.stateSince >= EVIDENCE_REFRESH_MS;
          if (next.quietStreak !== 0 || refresh) {
            next = {
              ...next,
              quietStreak: 0,
              stateSince: refresh ? at : next.stateSince,
            };
          }
        } else {
          const quietStreak = next.quietStreak + 1;
          next =
            quietStreak >= QUIET_FRAMES_TO_IDLE
              ? setCore(next, "idle", at)
              : { ...next, quietStreak };
        }
      } else if (next.coreState === "speaking" && speaking) {
        // TTS playback longer than 12s: speaking=true frames are the only
        // in-band evidence (response.speaking fires once at start).
        if (at - next.stateSince >= EVIDENCE_REFRESH_MS) {
          next = { ...next, stateSince: at };
        }
      }
      return next;
    }

    case "system.load": {
      return {
        ...s,
        gauges: {
          cpuPercent: num(env.data, "cpu_percent"),
          memUsedBytes: num(env.data, "mem_used_bytes"),
          memTotalBytes: num(env.data, "mem_total_bytes"),
          diskFreeBytes: num(env.data, "disk_free_bytes"),
          uptimeSecs: num(env.data, "uptime_secs"),
        },
      };
    }

    case "daemon.started": {
      const next: HudState = {
        ...s,
        daemonRoot: str(env.data, "root"),
        cloudKeyPresent: bool(env.data, "cloud_key_present"),
        inferenceOffline: false,
        lastError: null,
      };
      return setCore(next, "idle", at);
    }

    case "utterance.captured":
      return setCore(s, "processing", at);

    case "stt.transcript": {
      const text = str(env.data, "text");
      if (text === null) return s;
      return pushTranscript(setCore(s, "processing", at), {
        who: "user",
        text,
        ts: env.ts,
      });
    }

    case "stt.empty":
      // Nothing was said; pipeline ends here (opener orphaned daemon-side).
      return setCore(s, "idle", at);

    case "intent.classified": {
      const intent = str(env.data, "intent");
      if (intent === null) return s;
      return {
        ...s,
        lastIntent: {
          intent,
          confidence: num(env.data, "confidence") ?? 0,
          complexity: str(env.data, "complexity") ?? "",
        },
      };
    }

    case "route.local":
      return setCore(s, "thinking-local", at);

    case "route.cloud": {
      return setCore({ ...s, cloudModel: str(env.data, "model") }, "thinking-cloud", at);
    }

    case "route.cloud_failed": {
      // router.rs falls back to a local in-persona reply — keep thinking, cyan.
      const detail = str(env.data, "error") ?? "";
      return setCore(
        { ...s, lastError: { event: env.event, detail, ts: env.ts } },
        "thinking-local",
        at,
      );
    }

    case "route.completed": {
      const response = str(env.data, "response");
      if (response === null) return s;
      return pushTranscript(s, {
        who: "jarvis",
        text: response,
        ts: env.ts,
        routedTo: str(env.data, "routed_to") ?? undefined,
      });
    }

    case "response.speaking":
      return setCore(s, "speaking", at);

    case "pipeline.completed": {
      const timings: PipelineTimings = {
        sttMs: num(env.data, "stt_ms") ?? 0,
        classifyMs: num(env.data, "classify_ms") ?? 0,
        routeMs: num(env.data, "route_ms") ?? 0,
        speakMs: num(env.data, "speak_ms") ?? 0,
        firstAudioMs: num(env.data, "first_audio_ms"),
        totalMs: num(env.data, "total_ms") ?? 0,
      };
      // Turn complete: release the active agent so the core damps back to the
      // idle cyan (the constellation panel keeps showing the full roster).
      return setCore({ ...s, lastTimings: timings, activeAgent: null }, "idle", at);
    }

    case "route.failed": {
      const detail = str(env.data, "error") ?? "";
      return setCore(
        { ...s, lastError: { event: env.event, detail, ts: env.ts }, activeAgent: null },
        "idle",
        at,
      );
    }

    case "inference.unavailable": {
      const op = str(env.data, "op") ?? "";
      const detail = str(env.data, "error") ?? "";
      let next: HudState = {
        ...s,
        inferenceOffline: true,
        lastError: { event: env.event, detail: op ? `${op}: ${detail}` : detail, ts: env.ts },
      };
      // transcribe/classify abort the pipeline (reply.abandon in main.rs);
      // converse/generate fall back daemon-side and the pipeline continues.
      if (op === "transcribe" || op === "classify") {
        next = setCore(next, "idle", at);
      }
      return next;
    }

    case "action.executed": {
      const tool = str(env.data, "tool");
      const outcome = str(env.data, "outcome") ?? "";
      if (tool === null) return s;
      const seq = s.seq + 1;
      const actions = [{ tool, outcome, ts: env.ts, seq }, ...s.actions].slice(0, TICKER_CAP);
      return pushToast({ ...s, seq, actions }, "action", `ACTION: ${tool} — ${outcome}`, at);
    }

    case "memory.learned": {
      const key = str(env.data, "key");
      const value = str(env.data, "value") ?? "";
      if (key === null) return s;
      const seq = s.seq + 1;
      const facts = [{ key, value, ts: env.ts, seq }, ...s.facts].slice(0, TICKER_CAP);
      return pushToast({ ...s, seq, facts }, "learned", `LEARNED: ${key} = ${value}`, at);
    }

    case "memory.consolidated": {
      const upserts = num(env.data, "upserts") ?? 0;
      const deletes = num(env.data, "deletes") ?? 0;
      return pushToast(s, "memory", `MEMORY CONSOLIDATED: ${upserts} upserts, ${deletes} deletes`, at);
    }

    case "episodic.recorded": {
      // One completed turn's EPISODE-STORE outcome (Core-A). A telemetry-fed
      // TIMELINE entry — ACTIVITY only, never the episode body (the redacted
      // utterance/summary stay LOCAL in the daemon, recalled by voice). A
      // malformed payload (no `recorded` bool) is dropped rather than churning.
      const ep = parseEpisodicRecorded(env.data);
      if (ep === null) return s;
      const seq = s.seq + 1;
      const entry: EpisodeEntry = { recorded: ep.recorded, agent: ep.agent, ts: env.ts, seq };
      const timeline = [entry, ...s.memory.timeline].slice(0, EPISODE_TIMELINE_CAP);
      return {
        ...s,
        seq,
        memory: {
          ...s.memory,
          timeline,
          recordedCount: s.memory.recordedCount + (ep.recorded ? 1 : 0),
          gatedCount: s.memory.gatedCount + (ep.recorded ? 0 : 1),
        },
      };
    }

    case "user_model.consolidated": {
      // The user model's last consolidation (Core-B): how many bounded profile
      // entries the reflection pass wrote, folding recent episodes+facts. The
      // entries THEMSELVES are read by voice (user_model_query) — only the count
      // crosses the wire. A successful pass clears the "stale" flag.
      const um = parseUserModelConsolidated(env.data);
      if (um === null) return s;
      return {
        ...s,
        memory: {
          ...s.memory,
          userModelEntries: um.entriesWritten,
          userModelConsolidatedAt: env.ts,
          userModelStale: false,
        },
      };
    }

    case "user_model.consolidation_failed": {
      // The pass could not run this cycle (busy/locked DB). Honest "the profile
      // may be stale" affordance — the prior entry count stays shown, flagged.
      return { ...s, memory: { ...s.memory, userModelStale: true } };
    }

    case "memory.retention": {
      // The bounded evict-oldest retention pass. `episodesDeleted` is the PROOF
      // the episodic store is bounded (not "remembers everything"); surface it on
      // the memory panel. A pass that touched nothing (all-absent) is dropped.
      const ret = parseMemoryRetention(env.data);
      if (ret === null) return s;
      return {
        ...s,
        memory: {
          ...s.memory,
          lastEvictedEpisodes: ret.episodesDeleted,
          lastRetentionAt: env.ts,
        },
      };
    }

    case "proactive.brief": {
      // Proactive-learning contract: first-contact brief after an idle gap.
      const gap = num(env.data, "gap_hours");
      const habits = num(env.data, "habits_matched");
      const parts = ["PROACTIVE BRIEF"];
      if (gap !== null) parts.push(`GAP ${gap}H`);
      if (habits !== null) parts.push(`${habits} HABIT${habits === 1 ? "" : "S"} MATCHED`);
      return pushToast(s, "info", parts.join(" · "), at);
    }

    case "proactive.surface": {
      // EDITH's grounded proactive card (the SHIPPED default: speak OFF, so the
      // card is the only way the brief reaches the user). Surface it like
      // proactive.brief — a transient info toast. No text => no-op (no churn).
      const text = str(env.data, "text");
      if (text === null) return s;
      const trigger = str(env.data, "trigger");
      return pushToast(s, "info", trigger ? `EDITH (${trigger}): ${text}` : `EDITH: ${text}`, at);
    }

    case "proactive.suggestion": {
      // A propose-only PROACTIVE-INTELLIGENCE suggestion (#13 habit-automation
      // offer / #14 predictive suggestion) mined from the redacted, agent-scoped
      // episodic store. Surface it on the SUGGESTIONS feed — NEVER act on it
      // (every card carries auto_acts=false; the daemon only emits these with
      // [proactive] on, so with the feature OFF this case never fires).
      //
      // parseSuggestion drops anything the panel cannot render+address (no id,
      // unknown kind, or a habit offer with no proposed goal an Accept could route
      // through the gated standing path) — never fabricates a card from junk.
      const sg = parseSuggestion(env.data);
      if (sg === null) return s;
      // DEDUP: a dismissed id stays dismissed (suppress the re-offer); a re-emit
      // of an id already in the feed updates that card in place (newest evidence)
      // without duplicating it.
      if (s.dismissedSuggestions.has(sg.id)) return s;
      const without = s.suggestions.filter((x) => x.id !== sg.id);
      const suggestions = [sg, ...without].slice(0, SUGGESTION_CAP);
      return { ...s, suggestions };
    }

    case "proactive.digest": {
      // SMARTER BRIEF (#23): the daemon's PURE ranked/capped/cited digest
      // (brief.rs Brief::telemetry), emitted by agent.edith on the anticipation
      // tick. A DISTINCT event from the first-contact `proactive.brief` above and
      // the single-card `proactive.surface` — the daemon renamed it precisely so
      // those contracts are never reused/broken.
      //
      // parseProactiveDigest is DEFENSIVE: it drops any row with no honest line or
      // no real source (never fabricates a citation) and degrades a garbled/empty
      // payload to the honest-empty shape. The daemon only EMITS a non-empty
      // digest, so an empty parse means the payload was malformed — clear the
      // surface to null so the panel shows nothing (rather than a phantom shell)
      // and a later real digest replaces it. A real non-empty digest replaces the
      // prior one in place (the latest glance).
      const digest = parseProactiveDigest(env.data);
      if (digest.empty || digest.items.length === 0) {
        return s.proactiveDigest === null ? s : { ...s, proactiveDigest: null };
      }
      return { ...s, proactiveDigest: digest };
    }

    case "focus.active": {
      // FOCUS PROFILES (#24): the active focus posture (focus.rs
      // TunedBehavior::telemetry), emitted ONCE by agent.edith at the
      // anticipation-loop start. parseFocusActive PINS the permission-neutral
      // contract HUD-side — `permission_neutral` is forced true and
      // `raises_autonomy` / `loosens_gate` forced false — so a hostile/garbled
      // payload can NEVER flip the posture (a focus profile only ever quiets what
      // surfaces; it never loosens a gate, enables an action, or raises autonomy).
      // It never returns null, so the surface always reflects the latest posture;
      // the shipped "default" profile is the identity (today's behavior).
      const focus = parseFocusActive(env.data);
      return { ...s, focusProfile: focus };
    }

    case "standing.run": {
      // A standing mission completed a scheduled run. main.rs/standing.rs
      // promise "surfaces a standing.run HUD card"; surface it like
      // proactive.surface — a transient info toast. No goal => no-op (no churn).
      const goal = str(env.data, "goal");
      if (goal === null) return s;
      const report = str(env.data, "report");
      return pushToast(s, "info", report ? `STANDING: ${goal} — ${report}` : `STANDING: ${goal}`, at);
    }

    case "agent.active": {
      // Jarvis-Prime delegated to (or roll-call surfaced) an agent. Resolve
      // role/hue from the event, falling back to the static roster mirror so a
      // known agent always lights even on a minimal {name} event. Unknown
      // names still light (honesty: the daemon roster is truth) using the
      // event's own fields or the default cyan hue.
      const name = str(env.data, "name");
      if (name === null) return s;
      const profile = agentProfile(name);
      const role = str(env.data, "role") ?? profile?.role ?? "";
      const rawHue = num(env.data, "hue");
      const hue = normalizeHue(rawHue ?? profile?.hue ?? 190);
      const prev = s.activeAgent;
      // Anti-churn: identical active agent => same reference (the daemon may
      // re-emit agent.active mid-turn; React must be able to bail out).
      if (prev && prev.name === name && prev.role === role && prev.hue === hue) {
        return s;
      }
      return { ...s, activeAgent: { name, role, hue } };
    }

    case "heal.suppressed":
    case "heal.triggered": {
      return {
        ...s,
        heal: { event: env.event, errorsLast60s: num(env.data, "errors_last_60s") ?? 0 },
      };
    }

    case "heal.diagnosing": {
      // Root-cause diagnosis (v2): warn-amber "working" affordance. A new
      // diagnosis supersedes any prior pending proposal for a fresh burst.
      const signature = str(env.data, "signature") ?? "";
      const subsystem = str(env.data, "subsystem") ?? "";
      const files = strArr(env.data, "files") ?? [];
      return {
        ...s,
        healDiagnosing: { signature, subsystem, files, ts: env.ts },
        healProposal: null,
      };
    }

    case "heal.proposal": {
      const files = strArr(env.data, "files") ?? [];
      const validated = bool(env.data, "validated") ?? false;
      // Prefer the fields echoed on the event; fall back to the diagnosis that
      // led here so the panel always has a subsystem/signature to show.
      const diag = s.healDiagnosing;
      return {
        ...s,
        // The diagnosis resolved into a proposal — retire the "diagnosing"
        // affordance so the panel shows the proposal, not both.
        healDiagnosing: null,
        healProposal: {
          refTs: num(env.data, "ts"),
          files,
          validated,
          confidence: num(env.data, "confidence"),
          subsystem: str(env.data, "subsystem") ?? diag?.subsystem ?? "",
          signature: str(env.data, "signature") ?? diag?.signature ?? "",
          ts: env.ts,
        },
      };
    }

    case "heal.rejected": {
      // A rejection ends the current diagnose->propose attempt: clear the
      // pending surfaces and raise the RED error banner.
      return {
        ...s,
        healDiagnosing: null,
        healProposal: null,
        healAlert: {
          kind: "rejected",
          ts: env.ts,
          refTs: num(env.data, "ts"),
          files: [],
          detail: `STAGE: ${str(env.data, "stage") ?? "unknown"}`,
        },
      };
    }

    case "heal.blocked": {
      return {
        ...s,
        healDiagnosing: null,
        healAlert: {
          kind: "blocked",
          ts: env.ts,
          refTs: null,
          files: [],
          detail: str(env.data, "reason") ?? "unknown",
        },
      };
    }

    case "heal.applied": {
      // The (opt-in, dangerous) auto mode applied the patch live — the pending
      // proposal is consumed. RED banner: a live mutation is alert-worthy.
      return {
        ...s,
        healProposal: null,
        healDiagnosing: null,
        healAlert: {
          kind: "applied",
          ts: env.ts,
          refTs: num(env.data, "ts"),
          files: [],
          detail: "PATCH APPLIED — DAEMON RESTARTING",
        },
      };
    }

    case "forge.proposed": {
      // A validated, sandboxed micro-app was PROPOSED for human review. The
      // warn-amber "attention" panel (NOT an error). DEFENSIVE: a malformed
      // payload (missing name or ts) is NOT surfaced as a review card — the
      // panel must never show a proposal it cannot point an apply command at.
      // Drop a stale forge error banner now that a fresh proposal exists.
      const parsed = parseForgeProposed(env.data);
      if (parsed === null) return s;
      return {
        ...s,
        forgeAlert: null,
        forgeProposal: { name: parsed.name, ts: parsed.ts, at: env.ts },
      };
    }

    case "forge.rejected": {
      // A rejected draft was quarantined; nothing proposed. RED banner with the
      // failing stage. Clear any pending proposal (this attempt did not succeed).
      return {
        ...s,
        forgeProposal: null,
        forgeAlert: {
          kind: "rejected",
          ts: env.ts,
          detail: `STAGE: ${str(env.data, "reason") ?? "unknown"}`,
        },
      };
    }

    case "forge.blocked": {
      // The pipeline did not run. "disabled" is the shipped-OFF gate — NOT an
      // error, so it raises NO red banner (a no-op for the surfaces; the tool's
      // spoken reply already told the user it is off). Any other reason
      // (no_api_key, no_root, an abort stage) IS surfaced on the red banner.
      const reason = str(env.data, "reason") ?? "unknown";
      if (reason === "disabled") return s;
      return {
        ...s,
        forgeAlert: { kind: "blocked", ts: env.ts, detail: reason },
      };
    }

    case "code.explained": {
      // A code_explain ran (anthropic.rs::code_explain_tool). parseCodeExplained
      // NEVER returns null and surfaces ONLY real returned hits (the daemon never
      // fabricates a citation) + the method that ACTUALLY ran (so the panel never
      // claims neural when it fell back to BM25). An empty hits[] is the daemon's
      // {hits:0} HONEST "nothing indexed matched" — still shown, never hidden.
      // The new explanation REPLACES the last one (a fresh explain supersedes the
      // stale one); the pending proposal/note are kept (an explain is a different
      // action than a propose). NEVER carries a secret (question + cited chunks).
      const explained = parseCodeExplained(env.data);
      const prev = s.codeIntel;
      return {
        ...s,
        codeIntel: {
          explained,
          proposal: prev?.proposal ?? null,
          note: prev?.note ?? null,
        },
      };
    }

    case "code.proposed": {
      // A code_propose_diff wrote a REVIEWABLE diff to the proposal store
      // (anthropic.rs::code_propose_diff_tool). PROPOSE-ONLY: the user's tree is
      // untouched — the panel shows ONLY the MANUAL apply command. DEFENSIVE: a
      // payload with no finite `ts` is NOT surfaced (the panel must never show a
      // proposal it cannot point the apply command at). A fresh proposal clears a
      // stale rejected/blocked note. NEVER carries a secret (a <ts> + a count).
      const proposal = parseCodeProposed(env.data, env.ts);
      if (proposal === null) return s;
      const prev = s.codeIntel;
      return {
        ...s,
        codeIntel: {
          explained: prev?.explained ?? null,
          proposal,
          note: null,
        },
      };
    }

    case "code.rejected": {
      // The model's draft was NOT a usable/confined diff (non-diff prose, a
      // '..'/absolute escape, or oversize) — nothing was proposed and nothing was
      // changed. An HONEST, REVIEW-ONLY attention note (NOT the red alert chrome):
      // the propose-only contract held; the draft simply did not pass the gate.
      // Clears any pending proposal (this attempt produced none). SECRET-FREE.
      const reason = str(env.data, "reason") ?? "unknown";
      const prev = s.codeIntel;
      return {
        ...s,
        codeIntel: {
          explained: prev?.explained ?? null,
          proposal: null,
          note: { kind: "rejected", detail: reason, at: env.ts },
        },
      };
    }

    case "code.blocked": {
      // The tool did not run. "disabled" is the shipped-OFF gate — NOT an error
      // (the tool's spoken reply already said it is off), so it raises NO note (a
      // no-op for the surface, mirroring forge.blocked reason=disabled). Any other
      // reason (an abort stage) is an HONEST review-only note (not the red chrome).
      const reason = str(env.data, "reason") ?? "unknown";
      if (reason === "disabled") return s;
      const prev = s.codeIntel;
      return {
        ...s,
        codeIntel: {
          explained: prev?.explained ?? null,
          proposal: prev?.proposal ?? null,
          note: { kind: "blocked", detail: reason, at: env.ts },
        },
      };
    }

    case "shell.blocked": {
      // The sandboxed shell did NOT run a command. reason "disabled" is the
      // shipped-OFF / locked-down gate — the inert default the daemon's spoken
      // reply already named (NOT an error). reason "exec_failed" is the
      // device-gated exec seam erroring. Either way nothing ran and NO output is
      // surfaced — only the honest outcome. parseShellBlocked never throws.
      return { ...s, shell: { last: parseShellBlocked(env.data, env.ts) } };
    }

    case "shell.denied": {
      // A denylisted (destructive/exfil) command refused PRE-exec — it never
      // reached the gate, the park, or the exec. The `reason` names the matched
      // class. An HONEST refusal, not a fabricated result. SECRET-FREE.
      return { ...s, shell: { last: parseShellDenied(env.data, env.ts) } };
    }

    case "shell.preview": {
      // The DryRun FAITHFUL preview — the command is PARKED awaiting the user's
      // spoken confirm and has NOT run (every command is consequential and never
      // auto-runs). DEFENSIVE: a payload with no command text is dropped (the
      // panel must never show a phantom command); the prior outcome is kept.
      const out = parseShellCommandEvent("parked", env.data, env.ts);
      if (out === null) return s;
      return { ...s, shell: { last: out } };
    }

    case "shell.executing": {
      // Entering the Execute leg — reached ONLY after the full gate (master
      // switch ON + the spoken-confirm replay + voice-id + !lockdown). The
      // command is running; the faithful result follows in shell.ran. DEFENSIVE:
      // a payload with no command text is dropped; the prior outcome is kept.
      const out = parseShellCommandEvent("executing", env.data, env.ts);
      if (out === null) return s;
      return { ...s, shell: { last: out } };
    }

    case "shell.ran": {
      // The FAITHFUL real result — the honest exit code + timed-out / truncated
      // flags. There is deliberately NO output on the wire, so the panel NEVER
      // shows a (fabricable) command output. DEFENSIVE: a payload with no command
      // text is dropped; the prior outcome is kept. parseShellRan never throws.
      const out = parseShellRan(env.data, env.ts);
      if (out === null) return s;
      return { ...s, shell: { last: out } };
    }

    case "ui_actuate.blocked": {
      // Gated UI automation did NOT actuate. reason "disabled" is the shipped-OFF
      // / locked-down gate — the inert default the daemon's spoken reply already
      // named (NOT an error). Any other reason ("device_gated") is the device-
      // gated Accessibility-TCC seam refusing/failing. Either way nothing was
      // actuated and NO fabricated success is surfaced. parseUiActuateBlocked
      // never throws.
      return { ...s, uiActuate: { last: parseUiActuateBlocked(env.data, env.ts) } };
    }

    case "ui_actuate.refused": {
      // The PURE planner refused a degenerate / off-screen instruction PRE-
      // actuation — it never reached the gate, the park, or the actuation. The
      // `reason` names why. An HONEST refusal, not a fabricated result.
      return { ...s, uiActuate: { last: parseUiActuateRefused(env.data, env.ts) } };
    }

    case "ui_actuate.preview": {
      // The DryRun FAITHFUL per-action preview — the action is PARKED awaiting the
      // user's spoken confirm and has NOT been actuated (every actuation is
      // consequential + per-action gated; ONE confirm = ONE actuation; it never
      // auto-runs). DEFENSIVE: a payload with no action is dropped (the panel must
      // never show a phantom actuation); the prior outcome is kept.
      const out = parseUiActuateActionEvent("parked", env.data, env.ts);
      if (out === null) return s;
      return { ...s, uiActuate: { last: out } };
    }

    case "ui_actuate.actuating": {
      // Entering the Execute leg — reached ONLY after the full gate (master switch
      // ON + the spoken per-action confirm replay + voice-id + !lockdown) AND the
      // device Accessibility-TCC consent. The single action is being performed.
      // DEFENSIVE: a payload with no action is dropped; the prior outcome is kept.
      const out = parseUiActuateActionEvent("actuating", env.data, env.ts);
      if (out === null) return s;
      return { ...s, uiActuate: { last: out } };
    }

    case "ui_actuate.actuated": {
      // The FAITHFUL single-action result — one click/type/key on the named
      // target. NEVER fabricated. DEFENSIVE: a payload with no action is dropped;
      // the prior outcome is kept.
      const out = parseUiActuateActionEvent("actuated", env.data, env.ts);
      if (out === null) return s;
      return { ...s, uiActuate: { last: out } };
    }

    case "mcp.status": {
      // The MCP external-tool surface snapshot. parseMcpStatus NEVER returns null
      // (a malformed payload yields an empty, honest snapshot rather than a stale
      // one) and NEVER carries a secret — so the panel always renders the current
      // honest state: "off", "on but no servers", or the configured servers with
      // their connection status + tools + allowlists. REVIEW-ONLY.
      return { ...s, mcp: parseMcpStatus(env.data) };
    }
    case "capability.atlas": {
      // The unified ARMED/INERT capability surface (skills + agents + apps +
      // integrations). parseCapabilityAtlas NEVER returns null (a malformed
      // payload yields an empty, honest snapshot rather than a stale one) and
      // NEVER carries a secret — only capability names + credential PRESENCE.
      // REVIEW-ONLY.
      return { ...s, capabilityAtlas: parseCapabilityAtlas(env.data) };
    }

    case "tcc.snapshot": {
      // Ambient macOS app-privacy status (secret-free: availability + counts).
      // parseTccSnapshot NEVER returns null — an unreadable TCC store yields an
      // honest available=false, not a stale panel. REVIEW-ONLY.
      return { ...s, tccSentinel: parseTccSnapshot(env.data) };
    }

    case "tcc.anomaly": {
      // A batch of new-grant / denied->allowed escalation alerts. Each alert
      // fires ONCE (when the baseline first sees it), so ACCUMULATE newest-first,
      // dedupe, and cap. REVIEW-ONLY.
      const fresh = parseTccAnomalies(env.data);
      if (fresh.length === 0) return s;
      const merged = [...fresh, ...s.tccAnomalies]
        .filter((x, i, a) => a.indexOf(x) === i)
        .slice(0, TCC_ANOMALY_CAP);
      return { ...s, tccAnomalies: merged };
    }

    case "introspect.snapshot": {
      // Ambient sandboxed-child sentinel tally (secret-free counts). Never null —
      // a malformed payload yields an honest all-zero snapshot. REVIEW-ONLY.
      return { ...s, introspect: parseIntrospectSnapshot(env.data) };
    }

    case "introspect.profile_drift": {
      // The on-disk seatbelt profile was tampered/removed since launch — a
      // sandbox-integrity finding. Accumulate (deduped, capped). REVIEW-ONLY.
      const line = introspectDriftLine(env.data);
      if (line === null) return s;
      return { ...s, introspectAlerts: mergeIntrospectAlert(line, s.introspectAlerts) };
    }

    case "introspect.anomaly": {
      // A per-app RSS/CPU runaway the classifier flagged vs. its baseline.
      const line = introspectAnomalyLine(env.data);
      if (line === null) return s;
      return { ...s, introspectAlerts: mergeIntrospectAlert(line, s.introspectAlerts) };
    }

    case "introspect.module_violation": {
      // A dyld module an app loaded that its trust-on-first-use baseline never had
      // (injection / unexpected dlopen). REVIEW-ONLY — reported, never unloaded.
      const line = introspectModuleViolationLine(env.data);
      if (line === null) return s;
      return { ...s, introspectAlerts: mergeIntrospectAlert(line, s.introspectAlerts) };
    }

    case "introspect.security_event": {
      // A kernel security event about a tracked app from the (deferred, device-
      // gated) ES front-end — a W^X violation (jit=false app made memory
      // executable), a task-port acquisition (attach/inject), or a signal.
      // REVIEW-ONLY — surfaced, never blocked (the observer is NOTIFY-only).
      const line = introspectSecurityLine(env.data);
      if (line === null) return s;
      return { ...s, introspectAlerts: mergeIntrospectAlert(line, s.introspectAlerts) };
    }

    case "introspect.capabilities": {
      // The static per-app DECLARED-capability audit (from manifests). Replaces
      // the list wholesale each tick (it is a full inventory, not incremental).
      // REVIEW-ONLY and SECRET-FREE.
      return { ...s, introspectCapabilities: parseIntrospectCapabilities(env.data) };
    }

    case "attribution.health": {
      // Ambient capability-health snapshot (secret-free: counts + failing-flag
      // names). parseAttributionHealth NEVER returns null (a malformed payload
      // yields an honest all-zero snapshot). The latest snapshot REPLACES the
      // prior (it is the current health, not an accumulating log). PROPOSE-ONLY.
      return { ...s, attributionHealth: parseAttributionHealth(env.data) };
    }

    case "webhook.received": {
      // #35 webhook trigger decision (secret-free: outcome/event/intent ONLY —
      // NEVER the body/secret/signature). parseWebhookEvent drops a frame with a
      // missing/unrecognized outcome (un-actionable) and surfaces only the
      // mapping labels; we ACCUMULATE the running count + the last decision so
      // the panel can show listener liveness + the last intent. A consequential
      // mapping arrives as `parked` (it parked for the user's confirm — a webhook
      // NEVER auto-executes it). REVIEW-ONLY.
      const ev = parseWebhookEvent(env.data);
      if (ev === null) return s;
      return { ...s, webhooks: applyWebhookEvent(s.webhooks, ev) };
    }

    case "plugin.handshake": {
      // #36 register-on-launch handshake (secret-free: name/status/detail ONLY —
      // NEVER the capability token). parsePluginHandshake drops a frame with no
      // name or an unrecognized status; we ACCUMULATE the LATEST handshake per
      // module name (a re-launch updates in place). The panel lists the admitted,
      // validated, SBPL-sandboxed modules and surfaces a rejected one honestly.
      // REVIEW-ONLY.
      const rec = parsePluginHandshake(env.data);
      if (rec === null) return s;
      return { ...s, plugins: applyPluginHandshake(s.plugins, rec) };
    }

    case "security.status": {
      // The AT-REST ENCRYPTION posture snapshot. parseSecurityStatus NEVER returns
      // null (a malformed payload yields an honest, fail-OFF snapshot rather than a
      // stale one) and NEVER carries the master key — the wire has no key field, and
      // the parser surfaces only the booleans + the honest scope arrays + the
      // verbatim copy. The indicator renders ENCRYPTED AT REST / NOT ENCRYPTED from
      // the GROUND-TRUTH `active` (the key actually resolved), never from `config`
      // alone, so a config-on-but-key-failed session reads honestly as NOT ENCRYPTED.
      return { ...s, security: parseSecurityStatus(env.data) };
    }

    case "vision.describe": {
      // The ON-DEVICE VLM describe outcome (channel "local"). METADATA ONLY —
      // source kind + `available` (true ONLY when the on-device VLM actually
      // produced a description; false on every gate/confine/unavailable/transport
      // fall-back) + `vlm` (cfg.vision.enabled). The event carries NO pixels, NO
      // description text, and NO path: the visual content — the most sensitive
      // thing in the describe op — NEVER rides telemetry, so nothing visual lands
      // in state. A malformed payload (no usable `source`) is dropped rather than
      // wiping the last honest posture. The description text itself is spoken via
      // the persona-voiced reply + kept TRANSIENT, never persisted here.
      const vd = parseVisionDescribe(env.data);
      if (vd === null) return s;
      return { ...s, visionDescribe: vd };
    }

    case AUDIO_SOUND_MONITOR_EVENT: {
      // The OPT-IN ambient sound-monitor STATE (channel "local"), emitted once at
      // daemon startup from [audio].sound_monitor (SHIPS OFF + pinned).
      // parseAudioSoundMonitor NEVER returns null — a malformed payload yields the
      // honest fail-OFF snapshot (enabled:false) rather than a stale or fake
      // "monitoring" one. LABELS-ONLY by construction: this event carries the
      // monitor's on/off + consent posture, NEVER any audio or sound class. The
      // indicator renders MONITORING / OFF off `enabled`; `consent:"device_gated"`
      // states that even when opted in, continuous ambient capture is gated behind
      // macOS mic/TCC the daemon cannot grant.
      return { ...s, audioSoundMonitor: parseAudioSoundMonitor(env.data) };
    }

    case SCREEN_CONTEXT_CONFIGURED_EVENT: {
      // CONTINUOUS SCREEN CONTEXT (#42) startup snapshot (source "system"). Reads
      // the operator `enabled` opt-in (SHIPS FALSE — the loop never silently
      // arms), the hard ring `cap`, and the snapshot `interval_secs`. SECRET-FREE
      // — no glyphs cross this envelope. Folds into the OFF-default posture so the
      // WATCHING indicator + bounded copy render honestly before any snapshot.
      return {
        ...s,
        screenContext: applyScreenContextConfigured(s.screenContext, env.data),
      };
    }

    case SCREEN_CONTEXT_WATCHING_EVENT: {
      // Emitted on EACH continuous snapshot (source "system"). SECRET-FREE: reads
      // ONLY `watching` (the loop is active — the PROMINENT amber WATCHING
      // indicator), the BOUNDED ring counts `held`/`cap` (held N / cap M — never
      // the recognized glyphs), and `ingested` (whether THIS snapshot fed the
      // ring; false when the loop is OFF, so the OFF-default gate reads honestly).
      // Fails OFF — a malformed `watching` reads as NOT watching, never a fake
      // "watching". The recognized text NEVER rides this envelope (it lives only
      // in the daemon's transient ring), so nothing sensitive lands in state.
      return {
        ...s,
        screenContext: applyScreenContextWatching(s.screenContext, env.data),
      };
    }

    case SCREEN_CONTEXT_COMMAND_EVENT: {
      // A recall/forget VOICE command just ran (source "system"). SECRET-FREE:
      // reads ONLY the `verb` ("recall" | "forget" — NEVER the recalled redacted
      // text, which stays transient in the daemon + is rendered into the spoken
      // reply only) + the `enabled` opt-in echoed back. READ-ONLY/FORGETTABLE:
      // recall describes the bounded ring, forget wipes it; neither actuates.
      return {
        ...s,
        screenContext: applyScreenContextCommand(s.screenContext, env.data),
      };
    }

    case IMAGE_GENERATED_EVENT: {
      // The ON-DEVICE IMAGE-GENERATION outcome (channel "local", HUD-bound — it
      // NEVER rides the network). METADATA ONLY — `available` (true ONLY when the
      // on-device MLX diffusion model actually produced + saved an image; false on
      // every gate/unavailable/transport fall-back), the saved on-device `path`
      // (an abs path under state/images/), non-secret model/size/steps metadata,
      // and `image` (cfg.image.enabled). The event carries NO prompt and NO
      // pixels: the two most sensitive things in the op NEVER ride telemetry, and
      // the diffusion seed is intentionally dropped — so nothing visual / nothing
      // the user asked for lands in state. A malformed payload (not an object) is
      // dropped rather than wiping the last honest posture. parseImageGenerated
      // downgrades an "available but no path" payload to NOT available (no phantom
      // file). Image gen is LOCAL only — there is NEVER a cloud fall-back here.
      const ig = parseImageGenerated(env.data);
      if (ig === null) return s;
      return { ...s, imageGenerated: ig };
    }

    case "lockdown.status": {
      // The PANIC / LOCKDOWN emergency-stop posture snapshot (daemon emits it once
      // after telemetry::init, shipped default {locked:false, restored:false}).
      // parseLockdownStatus NEVER returns null (a malformed payload yields the
      // honest fail-SAFE snapshot) and carries booleans only. `locked` drives the
      // LOCKED DOWN / NORMAL indicator; `restoredFromMarker` true means this start
      // RE-ENTERED lockdown from the persisted marker (the stop survived a restart).
      return { ...s, lockdown: parseLockdownStatus(env.data) };
    }

    case "skills.catalog": {
      // The skills marketplace catalog snapshot. parseSkillsCatalog NEVER returns
      // null (a malformed payload yields an honest snapshot rather than a stale
      // one) and NEVER carries a secret — so the panel always renders the current
      // honest state: the in-tree library by category, the real counts, and the
      // [skills] on/off state. REVIEW-ONLY.
      return { ...s, skills: parseSkillsCatalog(env.data) };
    }

    case "eval.report": {
      // The periodic AGGREGATE-ONLY eval scorecard (eval.rs). parseEvalReport
      // NEVER returns null (a malformed payload yields an honest all-"awaiting
      // turns" snapshot rather than a stale one) and NEVER carries PII — so the
      // panel always renders the current honest state: measured latency/cost or
      // "awaiting turns", the routing/correction rates, and the OFF/propose
      // optimizer posture. REVIEW-ONLY: the eval framework measures, never tunes.
      return { ...s, evalReport: parseEvalReport(env.data) };
    }

    case "optimize.proposed": {
      // The propose-only optimizer wrote a REVIEWABLE routing proposal under
      // state/optimize/proposals/<ts>/ (optimize.rs run_optimizer). DEFENSIVE: a
      // malformed payload (no ts or no measured improvement) is NOT surfaced as a
      // review card — the panel must never show a proposal it cannot point an
      // apply command at. Nothing is applied; this only makes the gated MANUAL
      // command (scripts/apply_optimization.sh) visible.
      const proposal = parseOptimizerProposal(env.data);
      if (proposal === null) return s;
      return { ...s, optimizerProposal: proposal };
    }

    case "optimize.none":
    case "optimize.suppressed": {
      // A round that proposed NOTHING (no candidate beat the held-out baseline —
      // the can't-make-it-worse guarantee) OR the master switch is off
      // (suppressed). Clear any stale pending proposal so the panel stops
      // pointing at a superseded apply command. No-op (same reference) when none
      // is pending so a quiet round never churns the tree.
      if (s.optimizerProposal === null) return s;
      return { ...s, optimizerProposal: null };
    }

    case "docsearch.indexed": {
      // The on-device file-RAG index was (re)built over the EXPLICITLY-allowlisted
      // [docsearch].roots (router.rs::handle_docsearch_index). parseDocIndexStatus
      // NEVER returns null and carries COUNTS ONLY (files/chunks/embedded_chunks —
      // no path, no chunk text, no vector), so the panel always shows the current
      // honest index size and whether search will run neural (all chunks embedded)
      // or fall back to BM25. The event simply never arrives until the user enables
      // file search AND allowlists a folder AND runs an index.
      return { ...s, docIndex: parseDocIndexStatus(env.data) };
    }

    case "docsearch.searched": {
      // A CITED on-device file search ran (anthropic.rs::doc_search_tool).
      // parseDocSearchResult NEVER returns null and surfaces ONLY real returned
      // hits (the daemon never fabricates a citation) plus the method that ACTUALLY
      // ran (so the panel never claims neural when it fell back to BM25). An empty
      // hits[] is the honest "nothing found" — still shown, never hidden.
      return { ...s, docSearch: parseDocSearchResult(env.data) };
    }

    case "unified.searched": {
      // A UNIFIED personal search ran (anthropic.rs::unified_search_tool): one
      // query fanned out across every AVAILABLE source, then the pure
      // merge/rank/coverage core (unified_search::fold). parseUnifiedSearchResult
      // NEVER returns null and surfaces ONLY real returned hits (the daemon never
      // fabricates a hit/citation) + the HONEST coverage (searched vs skipped,
      // each skip with a reason — a skipped/disconnected source is never rendered
      // as if it had been searched). An empty hits[] with a non-empty searched set
      // is the honest "searched X, found nothing" — still shown, never hidden.
      // ON-DEVICE source content never leaves the device (this is the local
      // 127.0.0.1 broadcast only); CLOUD sources appear here ONLY when connected.
      return { ...s, unifiedSearch: parseUnifiedSearchResult(env.data) };
    }

    case "knowledge_graph.built": {
      // The gated knowledge-graph build ran (router.rs::handle_build_knowledge_graph):
      // the conservative deterministic-heuristic extractor mined the docsearch
      // chunks for grounded entities/relationships and upserted them into the
      // SHARED `user.world.*` tier. parseKnowledgeGraphResult NEVER returns null
      // and carries the build STATS (chunks scanned / written / skipped at the
      // bound), the honest extractor METHOD token, and the resulting bounded world
      // snapshot — entities grouped by type + their `source` provenance + the
      // relationships (each with its source detail). Every node/edge is EXTRACTED
      // from real document text and provenance-tagged (the daemon never fabricates
      // a node); an empty graph is the honest "extracted nothing". Counts/ids/
      // names/source strings only — no chunk text; the local 127.0.0.1 broadcast
      // only. The event simply never arrives until the user enables file search AND
      // [docsearch].build_graph AND says "map my documents".
      return { ...s, knowledgeGraph: parseKnowledgeGraphResult(env.data) };
    }

    case "answer.annotated": {
      // The HONEST per-turn answer provenance (anthropic.rs answer_annotation_
      // telemetry, emitted from main.rs run_pipeline). parseAnswerAnnotation NEVER
      // returns null (a malformed payload yields the honest empty shape rather than
      // a stale one) and is SECRET-FREE — it reads ONLY the real tool-result
      // sources (tool name + real locator + bounded snippet), the from-my-knowledge
      // flag, and the model's confidence self-report. Each source is a REAL tool
      // result the daemon recorded this turn — never fabricated; a turn with NO
      // retrieval carries the honest "from my own knowledge" flag, not a fake cite.
      //
      // The annotation is PER-TURN, so a fresh event REPLACES the prior one — a new
      // turn that used no retrieval must never keep showing the LAST turn's sources
      // (mirror of the daemon's per-turn accumulator guard that clears each turn).
      // An EMPTY annotation (the shipped default: both [answers] gates OFF, so empty
      // sources + null confidence + no from-my-knowledge label) clears the panel to
      // null so the HUD renders nothing; an already-null panel stays the SAME
      // reference so a stream of off-gate turns never churns the tree.
      const annotation = parseAnswerAnnotation(env.data);
      if (answerAnnotationIsEmpty(annotation)) {
        return s.answerAnnotation === null ? s : { ...s, answerAnnotation: null };
      }
      return { ...s, answerAnnotation: annotation };
    }

    case "answer.verified": {
      // The per-turn SELF-VERIFICATION outcome (anthropic.rs verify_telemetry,
      // emitted from main.rs run_pipeline next to answer.annotated). The OPTIONAL
      // second self-check ([answers].verify, ships OFF) critiques the DRAFT answer
      // ONCE against the real sources that turn used, then at most ONCE revises it;
      // this carries only the gate flag, the per-turn outcome token, the DERIVED
      // badge, and honest copy. SECRET-FREE: never the flagged-claim text (that
      // rides the answer when flagged), never content beyond the answer, never an
      // embedding/audio. parseVerifyStatus NEVER returns null (junk yields the
      // honest "off" shape) and DERIVES the badge from the validated outcome so a
      // spoofed wire badge can never disagree.
      //
      // PER-TURN, so a fresh event REPLACES the prior one — a later turn where the
      // pass did not run (or the gate is off) must never keep showing the LAST
      // turn's badge (mirror of the daemon's TurnVerifyGuard clearing each turn).
      // An EMPTY status (the shipped default: [answers].verify OFF => outcome "off"
      // + null badge) clears the panel to null so the HUD renders nothing; an
      // already-null panel stays the SAME reference so a stream of off-gate turns
      // never churns the tree.
      const verify = parseVerifyStatus(env.data);
      if (verifyStatusIsEmpty(verify)) {
        return s.verifyStatus === null ? s : { ...s, verifyStatus: null };
      }
      return { ...s, verifyStatus: verify };
    }

    case "answer.cross_checked": {
      // The per-turn TOOL-RESULT CROSS-CHECK outcome (#21,
      // anthropic.rs cross_check_badge_telemetry, emitted from main.rs
      // run_pipeline next to answer.verified). The BOUNDED plausibility
      // cross-check ([answers].cross_check, ships OFF) runs deterministic sanity
      // checks (and an OPTIONAL single bounded model pass) over a tool result
      // before it is surfaced as fact; a failed check DOWNGRADES confidence +
      // FLAGS the result, and NEVER removes a confirmation gate. This carries only
      // the gate flag, the per-turn outcome token, the DERIVED badge, and honest
      // copy. SECRET-FREE: never the raw tool result, never the flag-reason text
      // (that rides the answer when flagged), never content beyond the answer.
      // parseCrossCheckStatus NEVER returns null (junk yields the honest "off"
      // shape) and DERIVES the badge from the validated outcome so a spoofed wire
      // badge can never disagree.
      //
      // PER-TURN, so a fresh event REPLACES the prior one — a later turn where the
      // cross-check did not run (or the gate is off) must never keep showing the
      // LAST turn's badge (mirror of the daemon's TurnCrossCheckGuard clearing
      // each turn). An EMPTY status (the shipped default: [answers].cross_check OFF
      // => outcome "off" + null badge) clears the panel to null so the HUD renders
      // nothing; an already-null panel stays the SAME reference so a stream of
      // off-gate turns never churns the tree.
      const crossCheck = parseCrossCheckStatus(env.data);
      if (crossCheckStatusIsEmpty(crossCheck)) {
        return s.crossCheckStatus === null
          ? s
          : { ...s, crossCheckStatus: null };
      }
      return { ...s, crossCheckStatus: crossCheck };
    }

    case "answer.debated": {
      // The per-turn MULTI-MODEL DEBATE outcome (#22,
      // anthropic.rs debate_badge_telemetry, emitted from main.rs run_pipeline
      // next to answer.cross_checked). For GATED high-stakes asks only
      // ([answers].debate, ships OFF; a conservative should_debate predicate means
      // ordinary turns never debate), two brains answer the same question and the
      // daemon RECONCILES — at most two model calls. This carries only the gate
      // flag, the per-turn outcome token, the DERIVED badge, and honest copy.
      // SECRET-FREE: never the raw answers (when the brains disagree BOTH ride the
      // answer text), never content beyond the answer. parseDebateStatus NEVER
      // returns null (junk yields the honest "off" shape) and DERIVES the badge
      // from the validated outcome so a spoofed wire badge can never disagree.
      //
      // PER-TURN, so a fresh event REPLACES the prior one — a later ordinary turn
      // (or an off-gate turn) must never keep showing the LAST high-stakes turn's
      // badge (mirror of the daemon's TurnDebateGuard clearing each turn). An EMPTY
      // status (the shipped default / every ordinary turn => outcome "off" + null
      // badge) clears the panel to null so the HUD renders nothing; an already-null
      // panel stays the SAME reference so a stream of off-gate turns never churns
      // the tree.
      const debate = parseDebateStatus(env.data);
      if (debateStatusIsEmpty(debate)) {
        return s.debateStatus === null ? s : { ...s, debateStatus: null };
      }
      return { ...s, debateStatus: debate };
    }

    case "notebook.card": {
      // A RESEARCH NOTEBOOK voice command ran (daemon/src/notebook.rs dispatch,
      // emitted from router.rs). parseNotebookActivity NEVER returns null (a
      // malformed payload yields the honest "error/no card" shape rather than a
      // stale one) and is SECRET-FREE — it reads ONLY the verb, the topic, a
      // bounded already-redacted snippet, the saved-run count, and the REAL
      // fetched-source citations (run-local id + title + url, dropping any with
      // nothing to point at). The daemon PERSISTS a real run that ALREADY
      // happened and READS runs really saved — there is no fabricated source here.
      //
      // A verb that carries a card (saved/revisit/list/forget, including an
      // honest-empty revisit) REPLACES the prior activity. save_none/forget_none/
      // error carry NO card — a no-op the user already heard spoken — so we keep
      // the prior card in place rather than blanking the panel (READ-ONLY: a
      // failed/empty command must not erase the last real notebook surfaced). An
      // unknown verb yields card null, so it falls through to the keep-prior path
      // and never renders a bad badge.
      const activity = parseNotebookActivity(env.data);
      if (activity.card === null) {
        // Nothing new to surface; keep whatever real card we last showed.
        return s.notebook === null ? s : { ...s };
      }
      return { ...s, notebook: activity };
    }

    case "lifelog.digest": {
      // A LIFE-LOG voice command ran (daemon/src/lifelog.rs build_card, emitted
      // from router.rs). parseLifeLogDigest returns null ONLY when the period is
      // not a recognized label (an unparseable digest is dropped, never rendered
      // with a fabricated period). It is SECRET-FREE — every field is the
      // episodic store's already-redacted, bounded output: the period, the
      // honest-empty flag, the REAL recorded-episode count, the rendered digest
      // text, and the bounded themes / topics / recent summaries (non-string
      // entries dropped, lists capped).
      //
      // A fresh digest REPLACES the prior one (it is the latest read); an empty
      // window rides empty:true with a zero count + empty lists, surfaced by the
      // panel as the honest "nothing logged" state — never a fabricated event. A
      // malformed/unparseable payload is dropped (same reference) so junk never
      // churns the tree or shows a bad period.
      const digest = parseLifeLogDigest(env.data);
      if (digest === null) return s;
      return { ...s, lifelog: digest };
    }

    case "chart.data": {
      // A DATA->CHART spec was emitted (#41, daemon/src/chart.rs emit_chart, from
      // a "chart this" data path in router.rs). parseChartSpec returns null ONLY
      // when the `kind` is unrecognized (a chart with no known draw mode is
      // dropped, never rendered with a guessed mode); otherwise it yields the
      // EXACT series the daemon emitted — every point coerced verbatim ([x,y] with
      // both coords finite, a malformed point dropped not zero-filled), a series
      // with no usable point dropped, bounded by the VIEW caps. The Chart
      // component plots these points EXACTLY (line segments only between the GIVEN
      // points — NO interpolation, NO invented/extrapolated point) with axis
      // ranges derived from the data.
      //
      // A fresh spec REPLACES the prior one (it is the latest chart). `empty` is
      // re-derived from the surviving points (the parser never trusts the wire
      // flag), so an honest-empty spec rides empty:true and the panel shows the
      // honest-empty state rather than a fabricated point. A malformed/unrecognized
      // payload is dropped (same reference) so junk never churns the tree. The op
      // ships OFF ([chart].enabled), so nothing arrives until it is enabled.
      const spec = parseChartSpec(env.data);
      if (spec === null) return s;
      return { ...s, chart: spec };
    }

    case "report.built": {
      // A REPORT was assembled (#40, daemon/src/report.rs dispatch, emitted from
      // router.rs as report.built). parseReportReadout returns null when the
      // payload carries NO report object — the off/error verbs (report_off / error)
      // carry report:null, so the panel keeps showing nothing rather than a
      // fabricated shell. Otherwise it yields the title, the section + citation
      // counts, the bounded section headings, and the REAL citations (each coerced
      // item-by-item — a citation with no usable locator is dropped, never
      // fabricated; the daemon never synthesizes one). `empty` is re-derived (no
      // citation AND no section => honest-empty) so the panel surfaces the plain
      // "no sources to report on" rather than a fabricated body.
      //
      // A fresh report REPLACES the prior one (the latest build). A payload with no
      // report object is dropped (same reference) so an off/error round never
      // churns the tree or blanks a real report already shown. The op ships OFF
      // ([report].enabled), so nothing arrives until it is enabled. REVIEW-ONLY +
      // SECRET-FREE — counts/headings/real locators only.
      const readout = parseReportReadout(env.data);
      if (readout === null) return s;
      return { ...s, report: readout };
    }

    case "audit.snapshot": {
      // The authoritative AUDIT-LOG read (daemon/src/audit.rs recent(n) +
      // verify_chain() + len()). parseAuditSnapshot NEVER returns null (a
      // malformed payload yields an honest empty snapshot rather than a stale
      // one) and is SECRET-FREE by construction (only the redacted target +
      // decision/outcome survive — never the raw input, never the chain bytes).
      // A fresh snapshot REPLACES the prior one (it is the durable source of
      // truth); the live ring keeps reacting between snapshots. REVIEW-ONLY:
      // there is no action here that records, prunes, or rewrites the log.
      return { ...s, audit: parseAuditSnapshot(env.data) };
    }

    case "policy.snapshot": {
      // The authoritative POLICY read (daemon/src/policy.rs PolicyStore::rules()).
      // parsePolicySnapshot NEVER returns null (a malformed payload yields the
      // honest empty "ASK everywhere" snapshot rather than a stale one) and is
      // SECRET-FREE. A fresh snapshot REPLACES the prior one. READ-ONLY here:
      // the editor's writes go through the command channel (an explicit user
      // action), never by mutating this — there is no HUD path that sets a rule
      // without the user, mirroring the daemon's user-set-only invariant.
      return { ...s, policy: parsePolicySnapshot(env.data) };
    }

    case "policy.blocked":
    case "policy.auto_approved":
    case "confirm.parked": {
      // The LIVE consequential-gate chokepoint events — the immediate-reaction
      // surface BETWEEN authoritative audit.snapshot frames. SECRET-FREE: the
      // chokepoint payloads carry only {tool,agent} + an optional mcp/via marker
      // (never a target/input). Folded newest-first into a bounded ring; the
      // durable, hash-chained record stays daemon-side. A malformed event that
      // does not map to a gate verdict is ignored (same reference).
      const seq = s.seq + 1;
      const ev = liveGateEventFrom(env.event, env.data, env.ts, seq);
      if (ev === null) return s;
      return { ...s, seq, liveGate: [ev, ...s.liveGate].slice(0, LIVE_GATE_CAP) };
    }

    case "audit.truncated": {
      // A prune re-rooted the chain (audit.rs bounded retention). Reflect the
      // truncation flag on the current snapshot so the panel's chain copy stays
      // honest ("the surviving suffix still verifies from the new root") until
      // the next full audit.snapshot arrives. No-op if no snapshot is loaded yet
      // (the next snapshot will carry the authoritative truncated flag anyway).
      if (s.audit === null || s.audit.truncated) return s;
      return { ...s, audit: { ...s.audit, truncated: true } };
    }

    // Voice-id (on-device speaker verification) — daemon/src/voiceid.rs +
    // main.rs::handle_voice_id. Each helper folds ONLY the secret-free fields it
    // is told about into the prior status; the embedding/audio is NEVER on the
    // wire, so the indicator can never render one. voiceid.verify is the per-turn
    // verdict; the enroll_* events drive the ENROLLING/ENROLLED lifecycle.
    case "voiceid.verify":
      return { ...s, voiceId: applyVoiceIdVerify(s.voiceId, env.data) };
    case "voiceid.enroll_started":
      return { ...s, voiceId: applyVoiceIdEnrollStarted(s.voiceId, env.data) };
    case "voiceid.enroll_progress":
      return { ...s, voiceId: applyVoiceIdEnrollProgress(s.voiceId, env.data) };
    case "voiceid.enrolled":
      return { ...s, voiceId: applyVoiceIdEnrolled(s.voiceId) };
    case "voiceid.forgot":
      return { ...s, voiceId: applyVoiceIdForgot(s.voiceId) };

    // Model tier (model_tier.rs + router.rs) — MODEL-ONLY, changes no safety gate.
    // `model.tier` is the per-turn verdict (which model answered: local/fast/heavy
    // + why: override/auto/fallback); `model.swap` is a model-control voice command
    // pinning a tier or clearing to AUTO. Each folds only the secret-free wire
    // fields; an unknown tier/reason is ignored so a garbled frame never blanks the
    // indicator.
    case "model.tier":
      // The per-turn verdict ALSO carries the optional `local_sub` (which warm
      // local model answered this on-device turn when multi-resident), folded into
      // the resident-models surface as the ACTIVE sub-choice, AND the optional
      // inference-perf facts (speculative #37 / quant #39 — the path that actually
      // ran; throttle #38 — present only when the plan actually throttled, absent
      // under the OFF default), folded into the inference-perf surface.
      return {
        ...s,
        modelTier: applyModelTier(s.modelTier, env.data),
        localWarm: applyLocalSub(s.localWarm, env.data),
        inferencePerf: applyInferencePerf(s.inferencePerf, env.data),
      };
    case "model.swap":
      return { ...s, modelTier: applyModelSwap(s.modelTier, env.data) };

    // Resident local models (model_tier.rs::local_warm_telemetry, task #17 item 3)
    // — the config-derived warm-set PLAN for the Local tier. RESIDENT-MODELS-ONLY:
    // it reports which local models the policy keeps warm under the RAM budget +
    // whether multi-resident is in effect; it changes NO gate and does NOT change
    // which tier is chosen. HONEST: multi-resident keeps >1 local model warm for an
    // INSTANT swap ONLY when RAM allows (~2x RAM); single-resident is the safe
    // low-RAM default; it is the PLAN, never a measured speed benefit.
    case MODEL_LOCAL_WARM_EVENT:
      return { ...s, localWarm: applyLocalWarm(s.localWarm, env.data) };

    // Offline tool-loop (router.rs + anthropic.rs, task #3) — ACTIVITY-ONLY, the
    // ACTING-OFFLINE indicator. `local_tools.engaged` is the per-turn verdict (a
    // safe local tool actually ran offline; `gated`==true when a safety gate
    // parked/refused offline — the proof the gates apply offline). `executed` is a
    // per-tool activity trace; `out_of_subset` is the honest refusal of a tool the
    // 4B hallucinated outside the safe subset (never executed). These change NO gate
    // and carry no secret; model.tier already marks the Local tier this turn.
    case "local_tools.engaged":
      return { ...s, localTools: applyLocalToolsEngaged(s.localTools, env.data) };
    case "local_tools.executed":
      return { ...s, localTools: applyLocalToolsExecuted(s.localTools, env.data) };
    case "local_tools.out_of_subset":
      return { ...s, localTools: applyLocalToolsOutOfSubset(s.localTools, env.data) };

    // Voice tier (voice_tier.rs + speech.rs) — VOICE-ONLY, changes no safety gate.
    // `voice.tier` is the per-reply backend verdict (which TTS voiced it: ON-DEVICE
    // Kokoro vs the optional CLOUD ElevenLabs voices). Folds only the secret-free
    // wire fields {backend, agent}; an unknown backend is ignored so a garbled frame
    // never blanks the indicator. The payload carries NO key/voice id by contract.
    case "voice.tier":
      return { ...s, voiceTier: applyVoiceTier(s.voiceTier, env.data) };

    // STT tier (voice_tier.rs::resolve_stt_backend + speech.rs::resolve_transcribe_backend)
    // — which STT backend transcribed the last captured audio: ON-DEVICE whisper
    // (mlx_whisper, the private/offline default + the fallback on ANY cloud error)
    // vs the optional gated CLOUD ElevenLabs Scribe. STT is MORE sensitive than the
    // TTS text leg — the cloud path uploads the user's VOICE AUDIO. Folds only the
    // secret-free wire field {backend}; an unknown backend is ignored so a garbled
    // frame never blanks the indicator. The payload carries NO key/transcript/audio.
    case "stt.tier":
      return { ...s, sttTier: applySttTier(s.sttTier, env.data) };

    // #30 CONTINUOUS LIVE INTERPRETATION (interpret.rs + audio.rs). Two events
    // feed the read-only LIVE INTERPRET surface, both OFF by default ([interpret].live):
    //   - interpret.segment_fed {target, speak} at the audio.rs VAD-segment site —
    //     the DEVICE-GATED mic loop fed a segment into the PURE pipeline; marks the
    //     surface ACTIVE + records the direction/voicing.
    //   - interpret.segment {to, translated:true, spoke} from interpret_segment on a
    //     REAL translation (never an honest offline degrade) — bumps the real count.
    // SECRET-FREE: languages + booleans only; never the transcript or the translation.
    case "interpret.segment_fed":
      return { ...s, audioIo: applyInterpretSegmentFed(s.audioIo, env.data) };
    case "interpret.segment":
      return { ...s, audioIo: applyInterpretSegment(s.audioIo, env.data) };

    // #31 MULTI-SPEAKER DIARIZATION (main.rs transcript path + diarize.rs). Emitted on
    // the transcript path when [voice].diarize is ON. `backend_can_diarize` is the
    // GROUND-TRUTH honesty bit — false for on-device whisper (a single honest stream,
    // no diarization model — never a fabricated speaker), true only for the EL-Scribe
    // backend that carries speaker labels. We NEVER read the `transcript` text (it is
    // rendered in the comms panel, not this status surface), and never surface a
    // multi-speaker claim a non-diarizing backend could not have produced.
    case "transcript.diarized":
      return { ...s, audioIo: applyTranscriptDiarized(s.audioIo, env.data) };

    // #32 CUSTOM WAKE-WORD (wake.rs + audio.rs/router.rs). Emitted when an utterance is
    // DROPPED for lacking the configured wake phrase. Records the ACTIVE wake `phrase`
    // (default "jarvis") + that the gate has dropped something (the honest "gate is
    // live" signal). The `path` (a local wav path) is deliberately not surfaced.
    case "utterance.no_wake":
      return { ...s, audioIo: applyUtteranceNoWake(s.audioIo, env.data) };

    // Voice mode (prosody.rs::emit_telemetry) — the #33 adaptive-tone + #34
    // whisper EXPRESSIVENESS surface. `voice.prosody` is the per-reply delivery
    // verdict: the tone profile (neutral/calm/urgent/warm), the GROUND-TRUTH `rich`
    // bit (EL-v3 audio-tags/stability/style ACTUALLY applied — never faked on
    // Kokoro/non-v3), and the whisper state (terser + softer DELIVERY only). Folds
    // only the secret-free wire fields; an unknown profile degrades to neutral and a
    // rich:true claim on a non-EL backend is never honoured. EXPRESSIVENESS-ONLY:
    // changes no safety gate, and whisper never suppresses a required confirmation.
    // The payload carries NO key/voice id/text by contract.
    case "voice.prosody":
      return { ...s, voiceMode: applyVoiceMode(s.voiceMode, env.data) };

    case "app.started": {
      const name = str(env.data, "name");
      if (name === null) return s;
      if (s.runningApps.has(name)) {
        // Already tracked running; ensure the feed slice exists but otherwise
        // do not churn (a re-announce must not blank a populated panel).
        if (s.appFeeds[name]) return s;
        return {
          ...s,
          appFeeds: { ...s.appFeeds, [name]: emptyAppFeed(true) },
        };
      }
      const runningApps = new Set(s.runningApps);
      runningApps.add(name);
      const existing = s.appFeeds[name];
      return {
        ...s,
        runningApps,
        appFeeds: {
          ...s.appFeeds,
          // Preserve any prior items across a restart; just flip running on.
          [name]: existing ? { ...existing, running: true } : emptyAppFeed(true),
        },
      };
    }

    case "app.stopped": {
      const name = str(env.data, "name");
      if (name === null) return s;
      if (!s.runningApps.has(name) && !(s.appFeeds[name]?.running)) return s;
      const runningApps = new Set(s.runningApps);
      runningApps.delete(name);
      const existing = s.appFeeds[name];
      return {
        ...s,
        runningApps,
        // Keep the last items visible but mark the surface offline so the
        // panel can show its placeholder without losing context on a blip.
        appFeeds: existing
          ? { ...s.appFeeds, [name]: { ...existing, running: false } }
          : s.appFeeds,
      };
    }

    case "app.data": {
      const name = str(env.data, "name");
      if (name === null) return s;
      const payload = isObj(env.data.payload) ? env.data.payload : null;
      if (payload === null) return s;
      // An app is implicitly running once it relays data — but only adopt it
      // as a known surface; unknown names are simply stored under their key
      // (the panel decides which name it renders, ignoring the rest).
      const prev = s.appFeeds[name] ?? emptyAppFeed(true);
      let feed: AppFeed = { ...prev, running: true, updatedAt: at };

      // Stash the verbatim payload under its relay topic (apps.rs resolves an
      // app to one of its DECLARED topics, defaulting to "feed"). Topic-specific
      // panels (Silicon Canvas's canvas.* topics) read + narrow their own slice;
      // the feed-shaped fields below stay the global-scan "feed" view. A fresh
      // object so the prior `topics` map (shared via the shallow clone) is never
      // mutated in place.
      const topic = str(env.data, "topic") ?? "feed";
      feed.topics = { ...prev.topics, [topic]: payload };

      // "items" relay: brief + item list (+ fetched_at). Each field is
      // narrowed independently so a partial payload still applies what it can.
      if (Array.isArray(payload.items)) {
        feed.items = payload.items
          .filter(isObj)
          .map(coerceFeedItem)
          .slice(0, APP_FEED_ITEM_CAP);
      }
      const brief = str(payload, "brief");
      if (brief !== null) feed.brief = brief;
      const fetchedAt = str(payload, "fetched_at");
      if (fetchedAt !== null) feed.fetchedAt = fetchedAt;

      // "status" relay: feeds_ok / feeds_failed counters.
      const feedsOk = num(payload, "feeds_ok");
      if (feedsOk !== null) feed.feedsOk = feedsOk;
      const feedsFailed = num(payload, "feeds_failed");
      if (feedsFailed !== null) feed.feedsFailed = feedsFailed;

      const runningApps = s.runningApps.has(name)
        ? s.runningApps
        : (() => {
            const next = new Set(s.runningApps);
            next.add(name);
            return next;
          })();

      return { ...s, runningApps, appFeeds: { ...s.appFeeds, [name]: feed } };
    }

    case "app.op_forwarded": {
      // A voice command was translated into a structured op and forwarded to a
      // running micro-app (router.rs handle_silicon_canvas). Surface it like an
      // executed action — a provenance entry in the activity ticker + a
      // transient toast — so "show me the 3V3 net" leaves a visible trace of
      // the `select.net` op that went to the app. No name => ignore (no churn).
      const name = str(env.data, "name");
      if (name === null) return s;
      const opRaw = str(env.data, "op") ?? "";
      const op =
        opRaw.length > OP_FORWARD_OUTCOME_CAP
          ? `${opRaw.slice(0, OP_FORWARD_OUTCOME_CAP - 1)}…`
          : opRaw;
      const seq = s.seq + 1;
      const actions = [{ tool: name, outcome: op, ts: env.ts, seq }, ...s.actions].slice(
        0,
        TICKER_CAP,
      );
      return pushToast({ ...s, seq, actions }, "action", `OP → ${name}: ${op}`, at);
    }

    /* ---- #25 AUTO-DRAFT (draft.rs) — REVIEW-ONLY, NEVER auto-sent ---------- */
    case "draft.composed": {
      // A draft module produced a REVIEWABLE pending draft (status=draft). It is
      // a SUGGESTION the user reviews + sends — JARVIS never sends it from here
      // (the draft module has no send path). parseDraftComposed pins status to
      // "draft" and clips the preview HARD (the full body never rides the wire);
      // a payload with no usable id is dropped (nothing to key/forget).
      // Upsert by id (a re-composed draft for the same id replaces it).
      const draft = parseDraftComposed(env.data, env.ts);
      if (draft === null) return s;
      const drafts = [
        draft,
        ...s.actionSurface.drafts.filter((d) => d.id !== draft.id),
      ].slice(0, DRAFT_CAP);
      return { ...s, actionSurface: { ...s.actionSurface, drafts } };
    }

    case "draft.forgotten": {
      // The user discarded a pending draft. Drop it by id; idempotent (an unknown
      // id leaves the surface untouched, same reference). SECRET-FREE: id only.
      const id = str(env.data, "id");
      if (id === null) return s;
      if (!s.actionSurface.drafts.some((d) => d.id === id)) return s;
      const drafts = s.actionSurface.drafts.filter((d) => d.id !== id);
      return { ...s, actionSurface: { ...s.actionSurface, drafts } };
    }

    /* ---- #26 DURABLE MISSIONS (mission.rs) — load PAUSED, steps re-gated --- */
    case "mission.saved":
    case "mission.resumed":
    case "mission.cancelled": {
      // A durable mission's state changed. parseMissionEvent coerces a junk
      // status to the SAFE "paused" (never auto-active) and forces "cancelled"
      // for mission.cancelled; a payload with no usable id is dropped. Upsert by
      // id (a fresh lifecycle event for an existing mission REPLACES it in place,
      // moved to the front as the most-recently-touched). A persisted mission
      // NEVER auto-runs — it loads PAUSED and the user must explicitly resume,
      // and a resumed mission RE-GATES each consequential step (the persistence
      // carries no pre-approval). SECRET-FREE: id / goal / status / progress.
      const mission = parseMissionEvent(env.event, env.data, env.ts);
      if (mission === null) return s;
      const missions = [
        mission,
        ...s.actionSurface.missions.filter((m) => m.id !== mission.id),
      ].slice(0, MISSION_CAP);
      return { ...s, actionSurface: { ...s.actionSurface, missions } };
    }

    case "mission.blocked": {
      // The durable-missions feature did not run. "disabled" is the shipped-OFF
      // gate ([missions].durable=false) — NOT an error, so it is a no-op for the
      // surface (mirrors forge.blocked reason=disabled). Any other reason is also
      // not surfaced as a card here (there is nothing to persist); the spoken
      // reply already told the user. No-op (same reference).
      return s;
    }

    /* ---- #27 MACRO RECORD/REPLAY (router.rs) — stores no secrets, re-gated - */
    case "macro.recording_started": {
      // Recording began. We don't pre-create a card (the macro isn't recorded
      // until macro.recorded lands with a step count); the recording_started
      // event is surfaced via telemetry only. No-op for the surface.
      return s;
    }

    case "macro.recorded": {
      // A named macro was recorded — the daemon stored ONLY the intents/utterances
      // (NEVER a secret/token/credential). parseMacroRecorded drops a payload with
      // no usable name; the new entry resets the replay phase to idle. Upsert by
      // name (re-recording the same name replaces it). SECRET-FREE: name + count.
      const macro = parseMacroRecorded(env.data, env.ts);
      if (macro === null) return s;
      const macros = [
        macro,
        ...s.actionSurface.macros.filter((m) => m.name !== macro.name),
      ].slice(0, MACRO_CAP);
      return { ...s, actionSurface: { ...s.actionSurface, macros } };
    }

    case "macro.forgotten": {
      // The user deleted a macro. Drop it by name; idempotent (unknown name =>
      // same reference). SECRET-FREE: name only.
      const name = str(env.data, "name");
      if (name === null) return s;
      if (!s.actionSurface.macros.some((m) => m.name === name)) return s;
      const macros = s.actionSurface.macros.filter((m) => m.name !== name);
      return { ...s, actionSurface: { ...s.actionSurface, macros } };
    }

    case "macro.blocked": {
      // The macro feature did not run. "disabled" is the shipped-OFF gate
      // ([macros].enabled=false) — NOT an error, so it is a no-op for the surface
      // (mirrors forge.blocked reason=disabled). No-op (same reference).
      return s;
    }

    case "macro.replay_started": {
      // A macro replay began. Each recorded command will re-run through the NORMAL
      // router + the gate fresh (a consequential step is gated again, no batch
      // bypass). Mark the macro as replaying; an unknown name is a no-op (we only
      // track macros we have a recorded card for). Clears any stale last step.
      const name = str(env.data, "name");
      if (name === null) return s;
      if (!s.actionSurface.macros.some((m) => m.name === name)) return s;
      const macros = s.actionSurface.macros.map((m) =>
        m.name === name
          ? { ...m, replayPhase: "running" as const, lastStep: null, ts: env.ts }
          : m,
      );
      return { ...s, actionSurface: { ...s.actionSurface, macros } };
    }

    case "macro.replay_step": {
      // One recorded command was re-routed through the router + the gate. The
      // payload carries ONLY the recorded intent + the spoken utterance (the
      // daemon stored no secret, so neither can be one). Show it as live progress
      // on whichever macro is currently replaying. A step with neither field is
      // dropped. There is no `name` on a step event, so attach it to the macro in
      // the "running" phase.
      const step = parseMacroReplayStep(env.data);
      if (step === null) return s;
      if (!s.actionSurface.macros.some((m) => m.replayPhase === "running")) return s;
      const macros = s.actionSurface.macros.map((m) =>
        m.replayPhase === "running" ? { ...m, lastStep: step, ts: env.ts } : m,
      );
      return { ...s, actionSurface: { ...s.actionSurface, macros } };
    }

    case "macro.replay_done": {
      // The replay finished — every recorded command re-ran through the gate. Mark
      // the macro done (the panel reads "last replay: done"). Unknown name => the
      // running macro, if any (forward-tolerant); else no-op.
      const name = str(env.data, "name");
      const target =
        name !== null && s.actionSurface.macros.some((m) => m.name === name)
          ? (m: MacroEntry) => m.name === name
          : (m: MacroEntry) => m.replayPhase === "running";
      if (!s.actionSurface.macros.some(target)) return s;
      const macros = s.actionSurface.macros.map((m) =>
        target(m) ? { ...m, replayPhase: "done" as const, ts: env.ts } : m,
      );
      return { ...s, actionSurface: { ...s.actionSurface, macros } };
    }

    case "app.log":
    case "app.auth_failed":
    case "app.crashed":
      // Surfaced via telemetry/console only; not panel-state-bearing.
      return s;

    // Known but not state-bearing for the HUD.
    case "opener.played":
    case "opener.orphaned":
    case "intent.handled":
    case "vad.segment_capped":
      return s;

    default:
      // Unknown events must never throw — and must not churn state.
      return s;
  }
}
