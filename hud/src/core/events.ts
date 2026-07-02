/**
 * Telemetry wire format — transcribed from the daemon source (the code is
 * truth; see `daemon/src/telemetry.rs` and every `telemetry::emit` call site).
 *
 * Envelope (telemetry.rs::emit):
 *   { "ts": iso8601, "source": "audio"|"local"|"cloud"|"system",
 *     "event": str, "data": object }
 *
 * This module is pure TypeScript with no DOM, React, three.js, or Tauri
 * imports so it is verifiable headlessly under vitest.
 */

/** Sources the daemon emits today. Unknown sources are tolerated (the daemon
 *  will grow faster than the HUD), so the envelope keeps a plain string. */
export type KnownSource = "audio" | "local" | "cloud" | "system";

export interface TelemetryEnvelope {
  ts: string;
  source: string;
  event: string;
  data: Record<string, unknown>;
}

/* ------------------------------------------------------------------------ *
 * Payload shapes, one per emit call site (authoritative, from daemon/src).  *
 * ------------------------------------------------------------------------ */

/** audio / audio.level — capture loop, rate-limited to >=66ms (contract #1). */
export interface AudioLevelData {
  rms: number; // f32 rounded to 4dp
  speaking: boolean; // daemon's is_speaking() (mic muted while JARVIS talks)
}

/** system / system.load — telemetry.rs::system_load_task, every 2s. */
export interface SystemLoadData {
  cpu_percent: number;
  mem_used_bytes: number;
  mem_total_bytes: number;
  disk_free_bytes: number | null; // Option<u64> in SystemSnapshot
  uptime_secs: number;
}

/** system / daemon.started — main.rs. `cloud_key_present` added by contract #2. */
export interface DaemonStartedData {
  root: string;
  cloud_key_present?: boolean;
}

/** audio / utterance.captured — main.rs. */
export interface UtteranceCapturedData {
  path: string;
}

/** local / stt.transcript — main.rs. */
export interface SttTranscriptData {
  text: string;
}

/** local / stt.empty — main.rs. */
export interface SttEmptyData {
  path: string;
}

/** local / intent.classified — main.rs (Classification struct: inference.rs). */
export interface IntentClassifiedData {
  intent: string;
  confidence: number; // f64
  complexity: string;
}

/** local / route.local — router.rs. */
export interface RouteLocalData {
  intent: string;
  confidence: number;
}

/** cloud / route.cloud — router.rs. */
export interface RouteCloudData {
  intent: string;
  confidence: number;
  model: string;
  deep_reasoning: boolean;
}

/** cloud / route.cloud_failed — router.rs (daemon then degrades to local). */
export interface RouteCloudFailedData {
  intent: string;
  error: string;
}

/** local / intent.handled — router.rs. */
export interface IntentHandledData {
  intent: string;
  text: string;
}

/** local|cloud / route.completed — main.rs (source mirrors routed_to). */
export interface RouteCompletedData {
  routed_to: string; // "local" | "cloud"
  response: string;
}

/** system / route.failed — main.rs. */
export interface RouteFailedData {
  intent: string;
  error: string;
}

/** local / response.speaking — speech.rs (two call sites, same shape). */
export interface ResponseSpeakingData {
  text: string;
}

/** system / pipeline.completed — main.rs (PipelineTiming struct). */
export interface PipelineCompletedData {
  stt_ms: number;
  classify_ms: number;
  route_ms: number;
  first_audio_ms: number | null; // Option<u64>
  speak_ms: number;
  total_ms: number;
}

/** system / inference.unavailable — ops seen in daemon/src:
 *  transcribe, classify, converse, generate, extract_facts. */
export interface InferenceUnavailableData {
  op: string;
  error: string;
}

/** system / action.executed — router.rs + anthropic.rs (outcome capped 120ch). */
export interface ActionExecutedData {
  tool: string;
  outcome: string;
}

/** system / memory.learned — main.rs. */
export interface MemoryLearnedData {
  key: string;
  value: string;
}

/** system / memory.consolidated — reflect.rs. */
export interface MemoryConsolidatedData {
  upserts: number;
  deletes: number;
}

/** local / opener.played — speech.rs. */
export interface OpenerPlayedData {
  index: number;
  text: string | null;
}

/** local / opener.orphaned — speech.rs. */
export interface OpenerOrphanedData {
  reason: string;
  text: string | null;
}

/** audio / vad.segment_capped — audio.rs. */
export interface VadSegmentCappedData {
  samples: number;
  cap_secs: number;
}

/** system / heal.suppressed | heal.triggered — heal.rs. */
export interface HealData {
  errors_last_60s: number;
  reason?: string; // heal.suppressed
  action?: string; // heal.triggered
}

/** system / heal.diagnosing — heal.rs v2, emitted before drafting once a root
 *  cause is extracted from the ERROR burst (self-heal v2 contract A.1). */
export interface HealDiagnosingData {
  signature: string; // the extracted error signature
  files: string[]; // cited source files (src/<file>.rs[:line])
  subsystem: string; // module-path subsystem (audio/inference/router/...)
}

/** system / heal.proposal — heal.rs pipeline, mode=propose (self-heal contract).
 *  v2 adds the adversarial-review `confidence` (0..1) of the chosen candidate;
 *  `subsystem`/`signature` may be echoed from the diagnosis for the panel. */
export interface HealProposalData {
  ts: number; // staging/proposal unix timestamp (state/heal/proposals/<ts>/)
  files: string[]; // daemon source files the validated diff touches
  validated: boolean;
  confidence?: number; // v2: review confidence 0..1 (optional for older daemon)
  subsystem?: string; // v2: echoed from the diagnosis
  signature?: string; // v2: echoed from the diagnosis
}

/** system / heal.rejected — validation failed at some stage. */
export interface HealRejectedData {
  ts: number;
  stage: string; // "patch" | "cargo_check" | "cargo_test" | ...
}

/** system / heal.blocked — pipeline could not start (e.g. no cloud key). */
export interface HealBlockedData {
  reason: string; // "no_api_key" | ...
}

/** system / heal.applied — mode=auto applied the validated diff. */
export interface HealAppliedData {
  ts: number;
}

/* ------------------------------------------------------------------------ *
 * Self-Forge (forge.rs + the forge_app tool). MIRRORS the heal.* family.    *
 * The forge_app tool emits the HUD-facing forge.* events after running the   *
 * GATED, PROPOSE-ONLY pipeline (draft -> stage -> validate -> propose):      *
 *   - forge.proposed {name, ts} — a validated app is staged + PROPOSED for   *
 *     review (NOT installed, NOT running). The <ts> is the argument to the    *
 *     MANUAL deploy command scripts/apply_forge.sh <ts>.                      *
 *   - forge.rejected {reason}   — the draft failed a gate and was quarantined.*
 *   - forge.blocked  {reason}   — the pipeline did not run (e.g. "disabled",  *
 *     "no_api_key"). "disabled" is the shipped-OFF state, not an error.       *
 * SAFETY: a proposal is an ATTENTION state (review-only), NEVER an auto-apply.*
 * The HUD shows the manual command and makes clear nothing is installed yet.  *
 * ------------------------------------------------------------------------ */

/** system / forge.proposed — a validated micro-app PROPOSED for human review
 *  (state/forge/proposals/<ts>/). `name` is the forged app's name; `ts` is the
 *  <ts> for the MANUAL scripts/apply_forge.sh <ts> deploy command. The app is
 *  NOT installed and NOT running — it was only built + tested in a confined
 *  staging copy. */
export interface ForgeProposedData {
  name: string;
  ts: number;
}

/** system / forge.rejected — the draft failed a gate (parse/manifest/build/test)
 *  and was quarantined; nothing was proposed. `reason` is the failing stage. */
export interface ForgeRejectedData {
  reason: string;
}

/** system / forge.blocked — the pipeline did not run. `reason` is e.g.
 *  "disabled" (the shipped-OFF gate — not an error), "no_api_key", "no_root",
 *  or an abort stage. */
export interface ForgeBlockedData {
  reason: string;
}

/** A parsed, defensively-narrowed forge.proposed payload. Returns null unless
 *  BOTH a non-empty `name` string AND a finite `ts` number are present — a
 *  partial/garbled proposal event is NOT surfaced as a review card (the panel
 *  must never show a proposal it cannot point the user's apply command at).
 *  Never throws on junk. NEVER renders or carries a secret — `name`/`ts` only. */
export function parseForgeProposed(
  data: Record<string, unknown>,
): ForgeProposedData | null {
  const name = str(data, "name");
  const ts = num(data, "ts");
  if (name === null || name.length === 0 || ts === null) return null;
  return { name, ts };
}

/* ------------------------------------------------------------------------ *
 * CODE INTELLIGENCE (daemon/src/code.rs + the code_explain / code_propose_diff *
 * tools in anthropic.rs). The READ-ONLY / PROPOSE-ONLY code surface over the    *
 * user's OWN allowlisted codebase root. Mirrors the docsearch (cited hits) +    *
 * forge/heal (propose-only review, MANUAL apply command) postures. The daemon   *
 * emits, all over the local 127.0.0.1 broadcast, SECRET-FREE:                   *
 *   - code.explained {question, method, hits:[{file_path,byte_offset,snippet}]} *
 *       a GROUNDED, CITED answer over the real indexed code chunks. The         *
 *       persona ALSO speaks the prose answer; the wire carries the cited hits   *
 *       the answer was grounded in (the same real file+offset+snippet the       *
 *       docsearch panel already shows) so the panel can show the provenance.    *
 *   - code.explained {hits:0} — the HONEST not-indexed reply (empty/no-match    *
 *       index). NO hits array — nothing was cited because nothing matched.      *
 *   - code.proposed {ts, grounded_hits} — a REVIEWABLE unified diff was written *
 *       to the proposal store (state/code/proposals/<ts>/). PROPOSE-ONLY: the   *
 *       user's tree is untouched. <ts> is the argument to the MANUAL apply      *
 *       command scripts/apply_code_diff.sh <ts>; grounded_hits is how many real *
 *       indexed chunks the draft was grounded in.                               *
 *   - code.rejected {reason} — the model's draft was NOT a usable/confined diff *
 *       (non-diff prose / a '..'/absolute escape / oversize); nothing proposed. *
 *   - code.blocked {reason, tool} — the tool did not run. reason "disabled" is  *
 *       the shipped-OFF gate (NOT an error); any other reason is an abort stage.*
 * SAFETY: a proposal is REVIEW-ONLY (no auto-apply — the panel shows ONLY the   *
 * manual command). Explanations are grounded + cited; the model's code QUALITY  *
 * (does the diff compile/work) is runtime/model-gated and is NOT claimed here.  *
 * ------------------------------------------------------------------------ */

/** The ranking backend that ACTUALLY ran for a code explanation (the docsearch
 *  RankMethod over the code index, as_str): on-device neural cosine, or the
 *  lexical BM25 fallback when the on-device embedder was down. A future/unknown
 *  string is tolerated (rendered verbatim) so the panel never breaks. */
export type CodeMethod = "neural-embedding" | "lexical-bm25" | string;

/** One CITED code chunk an explanation was grounded in (code.explained `hits[]`,
 *  daemon docsearch DocHit). The citation anchor is `filePath` + `byteOffset`;
 *  `snippet` is the bounded chunk text the daemon already cited. Only ever built
 *  from a REAL returned hit — the parser drops any hit with no file to point at,
 *  so a fabricated citation can never be surfaced. NEVER carries a secret. */
export interface CodeCite {
  filePath: string;
  byteOffset: number;
  snippet: string;
}

/** A parsed `code.explained` payload. Two honest shapes share one event:
 *   - GROUNDED:    `hits` is a non-empty array of real cited chunks (+ the
 *                  question + the method that actually ran).
 *   - NOT-INDEXED: `hits` is the empty array (the daemon sent {hits:0}, or a
 *                  malformed/secret-laden payload that yields no real citation).
 *  The parser NEVER returns null and NEVER fabricates a hit — an empty `hits`
 *  IS the honest "nothing indexed matched", surfaced rather than hidden. */
export interface CodeExplained {
  question: string;
  method: CodeMethod;
  hits: CodeCite[];
}

/** A parsed `code.proposed` payload — a reviewable diff written to the proposal
 *  store. `ts` is the <ts> for the MANUAL scripts/apply_code_diff.sh <ts> apply
 *  command (the ONLY way the diff ever touches code); `groundedHits` is how many
 *  real indexed chunks the draft was grounded in. Returns null unless a finite
 *  `ts` is present — the panel must never show a proposal it cannot point the
 *  apply command at. NEVER carries a secret (ts + a count only). */
export interface CodeProposal {
  ts: number;
  groundedHits: number;
  at: string; // envelope ts of the code.proposed event
}

/** Coerce one untrusted code.explained hit into a CodeCite, or null if it has no
 *  usable `file_path` (the citation anchor — a hit with no file to cite is not a
 *  real citation, so it is dropped). Never throws. */
function coerceCodeCite(o: Record<string, unknown>): CodeCite | null {
  const filePath = str(o, "file_path");
  if (filePath === null || filePath.length === 0) return null;
  return {
    filePath,
    byteOffset: nonNegInt(o, "byte_offset"),
    snippet: str(o, "snippet") ?? "",
  };
}

/** Parse a `code.explained` payload into a CodeExplained. `hits` are coerced
 *  item-by-item (a hit with no file_path is dropped — not a citation); a missing
 *  or non-array `hits` (the {hits:0} not-indexed reply) yields the empty array.
 *  `method` defaults to "lexical-bm25" (the conservative fallback) so the panel
 *  never OVER-states a result as neural. NEVER returns null and NEVER fabricates
 *  a hit — an empty `hits` is the honest not-indexed state, shown not hidden. */
export function parseCodeExplained(data: Record<string, unknown>): CodeExplained {
  const rawHits = data["hits"];
  const hits = Array.isArray(rawHits)
    ? rawHits
        .filter(isPlainObject)
        .map(coerceCodeCite)
        .filter((h): h is CodeCite => h !== null)
    : [];
  return {
    question: str(data, "question") ?? "",
    method: str(data, "method") ?? "lexical-bm25",
    hits,
  };
}

/** Parse a `code.proposed` payload into a CodeProposal, or null when there is no
 *  finite `ts` to derive the MANUAL apply command from — a proposal the user
 *  cannot apply is never surfaced as a review card. `grounded_hits` defaults to
 *  0 (an honest "grounded in 0 chunks") when absent. Never throws. NEVER carries
 *  a secret — only the ts + the count survive. */
export function parseCodeProposed(
  data: Record<string, unknown>,
  at: string,
): CodeProposal | null {
  const ts = num(data, "ts");
  if (ts === null) return null;
  return { ts, groundedHits: nonNegInt(data, "grounded_hits"), at };
}

/** A human label for the code-explain ranking method that ACTUALLY ran — honest
 *  about whether the result was neural (on-device embeddings) or the BM25
 *  fallback. A future/unknown method string is shown verbatim (upper-cased). */
export function codeMethodLabel(method: CodeMethod): string {
  return method === "neural-embedding"
    ? "NEURAL (on-device embeddings)"
    : method === "lexical-bm25"
      ? "LEXICAL (BM25 keyword)"
      : method.toUpperCase();
}

/* ------------------------------------------------------------------------ *
 * SANDBOXED SHELL (#43) — daemon/src/anthropic.rs::shell_run_tool over the     *
 * deny-default sandbox-exec confinement (daemon/src/shell.rs). The HIGHEST-RISK *
 * feature: arbitrary code execution. It ships OFF, is treated as ALWAYS         *
 * consequential (it parks for the user's spoken confirm and NEVER auto-runs),   *
 * runs under a deny-default SBPL profile (no network, confined fs, NO secrets), *
 * and refuses a destructive-pattern denylist PRE-exec. The daemon emits, over   *
 * the local 127.0.0.1 broadcast, SECRET-FREE and NEVER FABRICATING a result:    *
 *   - shell.blocked {reason}       reason "disabled" = the OFF/locked gate (NOT  *
 *       an error — the inert default); "exec_failed" = the device-gated exec     *
 *       seam errored. Nothing was classified/parked/run in the "disabled" case.  *
 *   - shell.denied {reason}        a denylisted command refused PRE-exec, naming  *
 *       the matched destructive class — it never reached the gate/park/exec.      *
 *   - shell.preview {command}      the DryRun FAITHFUL preview the user confirms   *
 *       (the consequential-park surface shows this; the command is parked).        *
 *   - shell.executing {command}    entering the Execute leg AFTER the full gate    *
 *       (master switch ON + the spoken-confirm replay + voice-id + !lockdown).     *
 *   - shell.ran {command, exit_code, timed_out, truncated}  the FAITHFUL real      *
 *       result. There is deliberately NO output field on the wire — the panel      *
 *       NEVER shows a (potentially fabricated) command output, only the honest      *
 *       exit code + whether it timed out / was truncated.                           *
 * These mirror the code.* dot events, confirm.parked, and voiceid.denied           *
 * vocabulary the HUD already consumes. Every command is consequential: it PARKS    *
 * for a spoken confirm and NEVER auto-runs; OFF by default.                         *
 * ------------------------------------------------------------------------ */

/** The honest outcome of the last shell command the daemon spoke about, folded
 *  from one shell.* event. NEVER carries a command's output (the daemon never
 *  puts one on the wire) — only the command text, an outcome kind, a short
 *  reason, and (for a real run) the honest exit code / timed-out / truncated
 *  flags. SECRET-FREE. */
export type ShellOutcomeKind =
  /** shell.blocked reason=disabled — the OFF/locked gate. Inert default, NOT an
   *  error. Carries no command (nothing was classified/parked/run). */
  | "blocked-off"
  /** shell.blocked reason=exec_failed — the device-gated exec seam errored. */
  | "blocked-exec-failed"
  /** shell.denied — a denylisted command refused PRE-exec; `reason` names the
   *  matched destructive class. Never parked, never run. */
  | "denied"
  /** shell.preview — the DryRun faithful preview; the command is PARKED awaiting
   *  the user's spoken confirm. It has NOT run. */
  | "parked"
  /** shell.executing — entered the Execute leg after the full gate; the command
   *  is running (no result yet). */
  | "executing"
  /** shell.ran — the faithful real result: an honest exit code + timed-out /
   *  truncated flags. NO output is ever shown. */
  | "ran";

/** A parsed shell.* event — the last command's honest outcome. `command` is the
 *  exact command text the daemon already spoke (empty for the OFF gate, which
 *  carries none); `reason` is the short denylist class / exec-failure reason;
 *  the run fields are present ONLY for a "ran" outcome. NEVER an output. */
export interface ShellOutcome {
  kind: ShellOutcomeKind;
  /** The exact command (faithful, the daemon's own text). Empty when the event
   *  carries none (the OFF/locked gate). */
  command: string;
  /** A short reason — the matched denylist class (denied) or the exec-failure
   *  tag. Empty for outcomes that carry none. NEVER a secret. */
  reason: string;
  /** The real process exit code — ONLY for "ran"; null otherwise. */
  exitCode: number | null;
  /** Whether the run hit its timeout — ONLY meaningful for "ran". */
  timedOut: boolean;
  /** Whether the bounded output was truncated — ONLY meaningful for "ran". The
   *  output itself is NEVER on the wire; this is just the honest flag. */
  truncated: boolean;
  /** The envelope ts of the event. */
  at: string;
}

/** Parse a shell.blocked payload. reason "disabled" => the OFF/locked gate
 *  ("blocked-off", carries no command); any other reason (e.g. "exec_failed")
 *  => the exec seam errored ("blocked-exec-failed"). Never throws. */
export function parseShellBlocked(
  data: Record<string, unknown>,
  at: string,
): ShellOutcome {
  const reason = str(data, "reason") ?? "";
  const off = reason === "disabled";
  return {
    kind: off ? "blocked-off" : "blocked-exec-failed",
    command: "",
    reason: off ? "" : reason,
    exitCode: null,
    timedOut: false,
    truncated: false,
    at,
  };
}

/** Parse a shell.denied payload — a denylisted command refused PRE-exec. The
 *  `reason` names the matched destructive class (the panel surfaces it). Never
 *  throws. SECRET-FREE. */
export function parseShellDenied(
  data: Record<string, unknown>,
  at: string,
): ShellOutcome {
  return {
    kind: "denied",
    command: str(data, "command") ?? "",
    reason: str(data, "reason") ?? "unknown",
    exitCode: null,
    timedOut: false,
    truncated: false,
    at,
  };
}

/** Parse a shell.preview / shell.executing payload into a {parked|executing}
 *  outcome carrying the exact command. Returns null when there is no command
 *  text to show — the panel must never show a phantom command. Never throws. */
export function parseShellCommandEvent(
  kind: "parked" | "executing",
  data: Record<string, unknown>,
  at: string,
): ShellOutcome | null {
  const command = str(data, "command");
  if (command === null || command.length === 0) return null;
  return {
    kind,
    command,
    reason: "",
    exitCode: null,
    timedOut: false,
    truncated: false,
    at,
  };
}

/** Parse a shell.ran payload — the FAITHFUL real result. Returns null when there
 *  is no command text to attribute the result to. The exit code is the real
 *  process code (null when the daemon could not report one); timed_out /
 *  truncated default to false. NO output is ever parsed (none is on the wire) —
 *  the panel shows the honest exit code + flags, never a (fabricable) output.
 *  Never throws. */
export function parseShellRan(
  data: Record<string, unknown>,
  at: string,
): ShellOutcome | null {
  const command = str(data, "command");
  if (command === null || command.length === 0) return null;
  return {
    kind: "ran",
    command,
    reason: "",
    exitCode: num(data, "exit_code"),
    timedOut: bool(data, "timed_out") ?? false,
    truncated: bool(data, "truncated") ?? false,
    at,
  };
}

/* ------------------------------------------------------------------------ *
 * GATED UI AUTOMATION (#44, the CAPSTONE) —                                  *
 * daemon/src/anthropic.rs::ui_actuate_tool over the pure single-action       *
 * planner + the device-gated CGEvent/AX seam (daemon/src/ui_automation.rs).  *
 * The SINGLE MOST DANGEROUS feature: physically actuating the macOS UI       *
 * (click/type/key). It ships OFF, is treated as ALWAYS consequential and     *
 * PER-ACTION gated (it parks for the user's spoken confirm and NEVER         *
 * auto-runs; ONE confirm authorizes EXACTLY ONE actuation — a second         *
 * re-parks; never batched, never autonomous), performs exactly ONE action,   *
 * and the actuation itself is device-gated (Accessibility TCC consent). The  *
 * daemon emits, over the local 127.0.0.1 broadcast, SECRET-FREE and NEVER    *
 * FABRICATING a result:                                                      *
 *   - ui_actuate.blocked {reason}    reason "disabled" = the OFF/locked gate  *
 *       (NOT an error — the inert default; nothing planned/parked/actuated);   *
 *       "device_gated" = the Accessibility-TCC seam refused/failed on-device.  *
 *   - ui_actuate.refused {reason}    the PURE planner refused a degenerate /   *
 *       off-screen instruction PRE-actuation — it never parked, never acted.   *
 *   - ui_actuate.preview {action,target}  the DryRun FAITHFUL per-action       *
 *       preview the user confirms (the action is PARKED; ONE confirm = ONE     *
 *       actuation).                                                            *
 *   - ui_actuate.actuating {action,target}  entering the Execute leg AFTER     *
 *       the full gate (master switch ON + the spoken-confirm replay + voice-id *
 *       + !lockdown) AND the device TCC consent.                               *
 *   - ui_actuate.actuated {action,target}  the FAITHFUL single-action result.  *
 * Mirrors the shell.* / code.* vocabulary the HUD already consumes. Every      *
 * actuation is consequential + per-action gated; OFF by default.               *
 * ------------------------------------------------------------------------ */

/** The honest outcome of the last UI actuation the daemon spoke about, folded
 *  from one ui_actuate.* event. NEVER fabricates a result — only the action
 *  class ("click"/"type"/"key"), the faithful target description, an outcome
 *  kind, and a short reason. SECRET-FREE (no typed text on the wire). */
export type UiActuateOutcomeKind =
  /** ui_actuate.blocked reason=disabled — the OFF/locked gate. Inert default,
   *  NOT an error. Carries no action (nothing planned/parked/actuated). */
  | "blocked-off"
  /** ui_actuate.blocked reason=device_gated — the Accessibility-TCC seam
   *  refused (consent absent) or failed on-device. Nothing was actuated. */
  | "blocked-device"
  /** ui_actuate.refused — the pure planner refused a degenerate / off-screen
   *  instruction PRE-actuation; `reason` names why. Never parked, never acted. */
  | "refused"
  /** ui_actuate.preview — the DryRun faithful per-action preview; the action is
   *  PARKED awaiting the user's spoken confirm. ONE confirm = ONE actuation. */
  | "parked"
  /** ui_actuate.actuating — entered the Execute leg after the full gate + TCC
   *  consent; the single action is being performed. */
  | "actuating"
  /** ui_actuate.actuated — the faithful single-action result. */
  | "actuated";

/** A parsed ui_actuate.* event — the last actuation's honest outcome. `action`
 *  is the class ("click"/"type"/"key", empty for the OFF gate); `target` is the
 *  faithful control description; `reason` is the short blocked/refused reason.
 *  NEVER carries typed text, coordinates beyond what the daemon spoke, or a
 *  fabricated success. SECRET-FREE. */
export interface UiActuateOutcome {
  kind: UiActuateOutcomeKind;
  /** The action class the daemon already spoke ("click"/"type"/"key"). Empty
   *  when the event carries none (the OFF/locked gate / a planner refusal). */
  action: string;
  /** The faithful target description (the control the user named). Empty when
   *  the event carries none. */
  target: string;
  /** A short reason — the planner-refusal text or the device-gated tag. Empty
   *  for outcomes that carry none. NEVER a secret. */
  reason: string;
  /** The envelope ts of the event. */
  at: string;
}

/** Parse a ui_actuate.blocked payload. reason "disabled" => the OFF/locked gate
 *  ("blocked-off", carries no action); any other reason (e.g. "device_gated")
 *  => the device-gated Accessibility-TCC seam refused/failed ("blocked-device").
 *  Never throws. */
export function parseUiActuateBlocked(
  data: Record<string, unknown>,
  at: string,
): UiActuateOutcome {
  const reason = str(data, "reason") ?? "";
  const off = reason === "disabled";
  return {
    kind: off ? "blocked-off" : "blocked-device",
    action: "",
    target: "",
    reason: off ? "" : reason,
    at,
  };
}

/** Parse a ui_actuate.refused payload — the pure planner refused a degenerate /
 *  off-screen instruction PRE-actuation. `reason` names why (the panel surfaces
 *  it). Never parked, never actuated. Never throws. SECRET-FREE. */
export function parseUiActuateRefused(
  data: Record<string, unknown>,
  at: string,
): UiActuateOutcome {
  return {
    kind: "refused",
    action: "",
    target: "",
    reason: str(data, "reason") ?? "unknown",
    at,
  };
}

/** Parse a ui_actuate.preview / .actuating / .actuated payload into the matching
 *  outcome carrying the action class + faithful target. Returns null when there
 *  is no action to attribute (the panel must never show a phantom actuation).
 *  Never throws. SECRET-FREE. */
export function parseUiActuateActionEvent(
  kind: "parked" | "actuating" | "actuated",
  data: Record<string, unknown>,
  at: string,
): UiActuateOutcome | null {
  const action = str(data, "action");
  if (action === null || action.length === 0) return null;
  return {
    kind,
    action,
    target: str(data, "target") ?? "",
    reason: "",
    at,
  };
}

/** system / proactive.brief — first-contact brief (proactive contract). */
export interface ProactiveBriefData {
  gap_hours: number;
  habits_matched: number;
}

/** agent.edith / proactive.surface — EDITH's grounded HUD card (anticipate.rs Brief::telemetry). The SHIPPED default: with [proactive].speak OFF this card is the only way a proactive brief reaches the user. */
export interface ProactiveSurfaceData {
  trigger: string;
  text: string;
}

/* ------------------------------------------------------------------------ *
 * PROACTIVE-INTELLIGENCE SUGGESTIONS (daemon/src/proactive_intel.rs          *
 * Suggestion::telemetry() -> the `proactive.suggestion` feed card).          *
 *                                                                            *
 * The daemon's habit-detector / predictive-suggester mine ONLY the redacted, *
 * agent-scoped episodic store for a RECURRING pattern (>= a recurrence        *
 * threshold) and emit a PROPOSE-ONLY suggestion. Two kinds:                   *
 *                                                                            *
 *   - habit_automation — "you do X every weekday morning; make it a standing  *
 *     mission?" It carries the PROPOSED (NOT created) mission for HUD preview  *
 *     (proposed_goal + proposed_schedule, e.g. "daily at 09:00") plus         *
 *     accept_routes_through="standing_create" + auto_acts=false, so the panel  *
 *     copy can state the gated-accept / never-auto-act posture honestly.       *
 *     ACCEPTING it routes through the EXISTING gated standing-mission creation  *
 *     (a dedicated command verb) — there is NO ungated create here.            *
 *   - predictive — "you usually review the budget around now." Intel ONLY: it  *
 *     carries NO proposed_goal (a prediction has no action to accept).         *
 *                                                                            *
 * HONESTY (mirrors the daemon's posture, surfaced verbatim by the panel):     *
 * these are OBSERVED-pattern SUGGESTIONS (threshold-gated, never invented on   *
 * sparse/empty history), they can be WRONG and are DISMISSIBLE, JARVIS NEVER   *
 * auto-acts on them (auto_acts is always false), and accepting STILL goes      *
 * through the normal confirmation gate. SECRET-FREE: every field traces to     *
 * redacted, agent-scoped episodic data — there is no body/utterance/secret on  *
 * the wire. Parsed DEFENSIVELY — a card the panel cannot act on (no id, no     *
 * kind, a habit offer with no proposed goal) yields null, never a throw.       *
 * ------------------------------------------------------------------------ */

/** The kind discriminant the daemon stamps on a `proactive.suggestion` card.
 *  Kept as a closed union — an unknown kind is dropped by [`parseSuggestion`]
 *  (the panel renders Accept only for the kind it understands). */
export type SuggestionKind = "habit_automation" | "predictive";

/** The coarse time-of-day bucket a predictive suggestion recurs in (mirrors the
 *  daemon's morning/afternoon/evening split). Kept loose on the parsed shape so
 *  an unknown future bucket still surfaces as a plain label. */
export type SuggestionTimeOfDay = "morning" | "afternoon" | "evening";

/** One parsed `proactive.suggestion` feed card. Common fields are always
 *  present; the kind-specific fields are populated per `kind`. `autoActs` is
 *  ALWAYS false (JARVIS never auto-acts on a suggestion) — it is carried so the
 *  panel can state the never-auto-act posture from the wire, not a hard-code.
 *
 *  HABIT (kind="habit_automation"): `proposedGoal` + `proposedSchedule` are the
 *  PROPOSED (not created) mission shown for preview, and `acceptRoutesThrough`
 *  ("standing_create") makes the gated-accept route explicit. ACCEPT hands the
 *  proposed goal to the gated standing-mission creation verb — never a direct
 *  ungated create.
 *
 *  PREDICTIVE (kind="predictive"): carries `timeOfDay` + `occurrences` and NO
 *  proposed mission (`proposedGoal` is null) — a prediction has nothing to
 *  accept. */
export interface Suggestion {
  /** Stable content id — the dedup + Accept/Dismiss address. */
  id: string;
  /** The agent namespace the suggestion was mined under (its SCOPE). */
  agent: string;
  /** The honest, dismissible human line (daemon-authored, secret-free). */
  text: string;
  /** Which kind of card. Drives whether an Accept affordance is shown. */
  kind: SuggestionKind;
  /** The recurring topic/intent the suggestion is built from. */
  topic: string;
  /** How many times it recurred — the evidence, surfaced honestly. */
  occurrences: number;
  /** ALWAYS false: JARVIS never auto-acts on a suggestion. Carried from the
   *  wire so the panel's never-auto-act copy is grounded in the payload. */
  autoActs: boolean;
  /** habit only: the PROPOSED standing-mission goal (NOT created). null for a
   *  predictive suggestion (it has no action to accept). */
  proposedGoal: string | null;
  /** habit only: the PROPOSED schedule, human form (e.g. "daily at 09:00").
   *  null for a predictive suggestion. */
  proposedSchedule: string | null;
  /** habit only: the gated path an Accept routes through ("standing_create").
   *  null for a predictive suggestion. */
  acceptRoutesThrough: string | null;
  /** predictive only: the time-of-day bucket (morning/afternoon/evening). null
   *  for a habit offer. Kept as the raw wire string (loose). */
  timeOfDay: string | null;
}

/** Parse one `proactive.suggestion` payload (daemon Suggestion::telemetry()).
 *  Returns null unless the card is one the panel can render + address:
 *    - a non-empty `id` (the dedup + Accept/Dismiss key), AND
 *    - a known `kind` ("habit_automation" | "predictive"), AND
 *    - for a habit offer, a non-empty `proposed_goal` (an Accept with no goal
 *      could not route through the gated standing path — so it is NOT shown).
 *  Predictive cards never carry a proposed goal (it is forced null even if a
 *  hostile payload smuggled one — a prediction has no action to accept).
 *  `auto_acts` is read from the wire but pinned to false: a suggestion that
 *  claimed auto_acts=true would be a contract violation, so it is never
 *  honored. Defensive: missing optional fields default, junk yields null,
 *  never throws. SECRET-FREE — only the contracted fields survive. */
export function parseSuggestion(data: Record<string, unknown>): Suggestion | null {
  const id = str(data, "id");
  if (id === null || id.length === 0) return null;
  const kind = str(data, "kind");
  if (kind !== "habit_automation" && kind !== "predictive") return null;

  const agent = str(data, "agent") ?? "";
  const text = str(data, "text") ?? "";
  const topic = str(data, "topic") ?? "";
  const occurrences = num(data, "occurrences") ?? 0;

  if (kind === "habit_automation") {
    const proposedGoal = str(data, "proposed_goal");
    // A habit offer the Accept verb could not act on is not a renderable card.
    if (proposedGoal === null || proposedGoal.length === 0) return null;
    return {
      id,
      agent,
      text,
      kind,
      topic,
      occurrences,
      autoActs: false, // never honor a claimed auto_acts:true — pinned false.
      proposedGoal,
      proposedSchedule: str(data, "proposed_schedule"),
      acceptRoutesThrough: str(data, "accept_routes_through") ?? "standing_create",
      timeOfDay: null,
    };
  }

  // predictive — intel only, NO proposed mission (forced null).
  return {
    id,
    agent,
    text,
    kind,
    topic,
    occurrences,
    autoActs: false,
    proposedGoal: null,
    proposedSchedule: null,
    acceptRoutesThrough: null,
    timeOfDay: str(data, "time_of_day"),
  };
}

/** Build the natural-language standing-mission request an ACCEPT on a habit
 *  offer sends to the daemon. ACCEPT is NOT an ungated create: the request is
 *  phrased as a HARD-recurring standing-mission setup ("Set up a standing
 *  mission to <goal>, <schedule>"), which the daemon's selector routes to
 *  `standing_create` — and `standing_create` PARKS behind the cross-turn
 *  confirmation gate. So accepting still goes through the normal gate; nothing is
 *  established until the user confirms there too.
 *
 *  Returns null for a NON-acceptable suggestion (a predictive card has no
 *  proposed goal — there is nothing to accept), so the panel can hide Accept
 *  exactly when the daemon carried no action. Pure + secret-free (the goal +
 *  schedule are daemon-authored, redacted-episodic-derived strings). */
export function suggestionAcceptText(s: Suggestion): string | null {
  if (s.kind !== "habit_automation" || s.proposedGoal === null) return null;
  const goal = s.proposedGoal.trim();
  if (goal.length === 0) return null;
  const schedule = s.proposedSchedule?.trim();
  // "Set up a standing mission to <goal>, <schedule>." — the explicit
  // standing-setup phrasing + the schedule's hard recurring cue both point the
  // selector at the gated Standing route (never a one-shot, never ungated).
  return schedule && schedule.length > 0
    ? `Set up a standing mission to ${goal}, ${schedule}.`
    : `Set up a standing mission to ${goal}.`;
}

/* ------------------------------------------------------------------------ *
 * SMARTER BRIEF — the proactive DIGEST (#23; daemon/src/brief.rs            *
 * Brief::telemetry() -> the `proactive.digest` card, emitted from main.rs's  *
 * anticipation tick by agent.edith). A DISTINCT event from the first-contact *
 * `proactive.brief{gap_hours, habits_matched}` (proactive.rs) AND from the   *
 * single-card `proactive.surface` — the daemon renamed it precisely so the   *
 * HUD's existing proactive.brief contract is never reused/broken.            *
 *                                                                            *
 * The daemon's build_brief is a PURE ranker: it takes the verified, INJECTED *
 * signal snapshot (calendar/mail/health/market/news/routine/critical), DROPS *
 * any signal with no real citation (it refuses to fabricate a source),       *
 * RELEVANCE-RANKS (Urgent > Important > Routine), CAPS to a glance (focus     *
 * verbosity may tighten the cap), and renders an HONEST-EMPTY digest when     *
 * nothing survives. Each surviving item carries its REAL source citation      *
 * (the signal's origin, e.g. "calendar:evt_9" / "gmail:msg_42" /             *
 * "global_scan:reuters-1") — never invented.                                 *
 *                                                                            *
 * HONESTY (mirrors the daemon, surfaced verbatim by the panel): every item   *
 * cites a real connected source; an UNCONNECTED source contributes no signal  *
 * (honestly absent, never padded); when there are no signals the digest is    *
 * HONESTLY EMPTY ("nothing to brief"). The daemon only emits this event when  *
 * the digest is NON-empty, so the parser's honest-empty shape is reached only *
 * when a payload arrives malformed/empty (defensive). SECRET-FREE: only a     *
 * priority label + an honest line + a rendered citation ride the wire — no    *
 * body/utterance/embedding/secret. Parsed DEFENSIVELY: a row with no text or  *
 * no real source is dropped (never a fabricated citation); junk yields the    *
 * honest-empty digest, never a throw.                                        *
 * ------------------------------------------------------------------------ */

/** The priority label the daemon stamps on a digest row (brief.rs Priority,
 *  lowercase on the wire). Kept as a closed union; an unknown priority is
 *  normalized to "routine" (the lowest rank) so a novel/garbled label can never
 *  masquerade as more urgent than it is — fail QUIET, not loud. */
export type BriefPriority = "urgent" | "important" | "routine";

/** One parsed `proactive.digest` row: its priority, the honest line, and the
 *  REAL rendered source citation (e.g. "calendar:evt_9"). All three are required
 *  for a renderable row — a row with no line or no source is dropped by
 *  [`parseProactiveDigest`] (a citation is the honesty anchor; without it the row
 *  would be an uncited claim, which JARVIS does not surface). */
export interface BriefItem {
  /** The relevance priority — drives the row's rank chip. */
  priority: BriefPriority;
  /** The honest one-line text (daemon-authored, grounded in the source). */
  text: string;
  /** The REAL rendered source citation ("source:ref_id"). Always non-empty on a
   *  surviving row (the parser dropped any row without one). */
  source: string;
}

/** A complete, defensively-parsed `proactive.digest` payload — the HUD's view of
 *  the ranked/capped/cited brief. `empty` is the HONEST-EMPTY flag; `items` are
 *  the surviving cited rows in the daemon's already-ranked order. With no
 *  surfacable signal this is `{ empty: true, items: [] }` — the honest "nothing
 *  to brief" state, never padded. */
export interface ProactiveDigest {
  empty: boolean;
  items: BriefItem[];
}

/** Normalize a wire priority string to the closed [`BriefPriority`] union. An
 *  unknown/garbled value floors to "routine" (lowest rank) — a novel label can
 *  never inflate a row's urgency. */
function normalizeBriefPriority(v: string | null): BriefPriority {
  return v === "urgent" || v === "important" || v === "routine" ? v : "routine";
}

/** Coerce one untrusted digest-row object into a [`BriefItem`], or null when it
 *  lacks a usable `text` OR a usable `source` — an uncited or empty row is NOT
 *  surfaced (the digest cites real sources only; it never fabricates one). The
 *  priority floors to "routine" on an unknown label. Only the three contracted
 *  fields are read — never a secret. Never throws. */
function coerceBriefItem(o: Record<string, unknown>): BriefItem | null {
  const text = str(o, "text");
  if (text === null || text.trim().length === 0) return null;
  const source = str(o, "source");
  if (source === null || source.trim().length === 0) return null;
  return { priority: normalizeBriefPriority(str(o, "priority")), text, source };
}

/** Parse a `proactive.digest` payload (daemon Brief::telemetry()) into a
 *  [`ProactiveDigest`]. `items` are coerced row-by-row — a row with no honest
 *  line or no real source is DROPPED (never fabricated), preserving the daemon's
 *  already-ranked order for the survivors. The digest is honest-empty (`empty:
 *  true, items: []`) when the wire says `empty` OR when NO row survived coercion
 *  — so a malformed/garbled payload degrades to the honest "nothing to brief"
 *  state rather than a padded one. NEVER returns null + never throws — a
 *  proactive.digest event always yields an honest digest. SECRET-FREE: only the
 *  priority/line/citation survive. */
export function parseProactiveDigest(data: Record<string, unknown>): ProactiveDigest {
  const rawItems = data["items"];
  const items = Array.isArray(rawItems)
    ? rawItems
        .filter(isPlainObject)
        .map(coerceBriefItem)
        .filter((it): it is BriefItem => it !== null)
    : [];
  // Honest-empty when the wire flags it OR nothing survived: an empty/garbled
  // digest is "nothing to brief", never padded into a phantom item.
  const empty = (bool(data, "empty") ?? false) || items.length === 0;
  return { empty, items: empty ? [] : items };
}

/** Human label for a brief-row priority chip — the relevance rank, honestly
 *  framed. Shared by the panel so the copy is unit-testable + consistent. */
export function briefPriorityLabel(p: BriefPriority): string {
  switch (p) {
    case "urgent":
      return "URGENT";
    case "important":
      return "IMPORTANT";
    case "routine":
      return "ROUTINE";
  }
}

/* ------------------------------------------------------------------------ *
 * FOCUS PROFILES — the active focus posture (#24; daemon/src/focus.rs        *
 * TunedBehavior::telemetry(profile) -> the `focus.active` card, emitted once  *
 * from main.rs's anticipation-loop start by agent.edith).                    *
 *                                                                            *
 * A focus profile (default | work | sleep | deep_focus | a named custom) is a *
 * PERMISSION-NEUTRAL lens: apply_profile adjusts ONLY non-consequential knobs *
 * — which signal CATEGORIES surface, brief VERBOSITY, and whether SUGGESTIONS *
 * are quieted. It can only ever make JARVIS QUIETER, never more permissive.   *
 * By CONSTRUCTION the daemon's TunedBehavior carries NO gate/permission/      *
 * autonomy field, so the card cannot leak one — and it states the contract    *
 * explicitly on the wire (permission_neutral / raises_autonomy / loosens_gate) *
 * so the HUD copy is GROUNDED in the payload, not a hardcode.                 *
 *                                                                            *
 * The shipped default ([focus].profile = "default") is the IDENTITY: every    *
 * category surfaces, full verbosity, suggestions not quieted — today's        *
 * behavior byte-for-byte. SECRET-FREE: only the profile name + category labels *
 * + the booleans ride the wire. Parsed DEFENSIVELY: a card that claimed        *
 * raises_autonomy / loosens_gate is PINNED to the neutral truth (the contract  *
 * is enforced HUD-side too — a hostile payload can never flip the posture);    *
 * junk degrades to the default-identity posture, never a throw.               *
 * ------------------------------------------------------------------------ */

/** The verbosity the active focus applies to the brief (focus.rs Verbosity,
 *  lowercase on the wire). Closed union; an unknown value normalizes to "full"
 *  (the loosest = today's default), so a garbled label never silently tightens
 *  the readout into claiming the brief is quieter than it is. */
export type FocusVerbosity = "full" | "brief" | "silent";

/** A complete, defensively-parsed `focus.active` payload — the HUD's view of the
 *  active focus posture. `surfacing` are the signal categories that DO surface
 *  under this profile (the rest are quieted). The three posture booleans are the
 *  PERMISSION-NEUTRAL contract, read from the wire but PINNED to the only honest
 *  values (a profile can never raise autonomy or loosen a gate — so even a
 *  hostile payload is forced to the neutral truth here). */
export interface FocusActive {
  /** The active profile name (default | work | sleep | deep_focus | custom). */
  profile: string;
  /** The signal categories that surface under this profile (others are quieted).
   *  Non-string entries are dropped; an empty list is the most-quiet posture. */
  surfacing: string[];
  /** The brief verbosity the profile applies (full | brief | silent). */
  verbosity: FocusVerbosity;
  /** Whether the profile quiets the suggestion feed. */
  suggestionsQuieted: boolean;
  /** ALWAYS true: a focus profile is permission-neutral by construction. Read
   *  from the wire but pinned — the HUD states the posture from the payload. */
  permissionNeutral: boolean;
  /** ALWAYS false: a profile NEVER raises autonomy. Pinned (a claimed `true` is
   *  a contract violation and is not honored). */
  raisesAutonomy: boolean;
  /** ALWAYS false: a profile NEVER loosens a gate. Pinned (a claimed `true` is a
   *  contract violation and is not honored). */
  loosensGate: boolean;
}

/** Normalize a wire verbosity string to the closed [`FocusVerbosity`] union. An
 *  unknown value normalizes to "full" (today's default) — a garbled label never
 *  understates how much surfaces. */
function normalizeFocusVerbosity(v: string | null): FocusVerbosity {
  return v === "full" || v === "brief" || v === "silent" ? v : "full";
}

/** Parse a `focus.active` payload (daemon TunedBehavior::telemetry()) into a
 *  [`FocusActive`]. Defensive: a missing `profile` defaults to "default" (the
 *  identity); `surfacing` drops non-string entries; `verbosity` normalizes to
 *  "full" on an unknown label; `suggestions_quieted` defaults false. The three
 *  posture booleans are PINNED to the only honest values — `permission_neutral`
 *  is forced true and `raises_autonomy` / `loosens_gate` are forced false, so a
 *  hostile/garbled payload can NEVER flip the permission-neutral contract HUD-side
 *  (the panel copy stays grounded in the truth, not the wire's claim). NEVER
 *  returns null + never throws — a focus.active event always yields an honest
 *  posture. SECRET-FREE: only the name + category labels + booleans survive. */
export function parseFocusActive(data: Record<string, unknown>): FocusActive {
  const profile = str(data, "profile");
  return {
    profile: profile !== null && profile.trim().length > 0 ? profile : "default",
    surfacing: strArr(data, "surfacing") ?? [],
    verbosity: normalizeFocusVerbosity(str(data, "verbosity")),
    suggestionsQuieted: bool(data, "suggestions_quieted") ?? false,
    // Pin the permission-neutral contract HUD-side: a profile can NEVER raise
    // autonomy or loosen a gate, so we do not honor a payload that claimed it.
    permissionNeutral: true,
    raisesAutonomy: false,
    loosensGate: false,
  };
}

/** True when a focus posture is the DEFAULT IDENTITY — today's behavior:
 *  full verbosity, suggestions not quieted, and the full category set surfacing
 *  (the daemon's BaseBehavior surfaces all seven categories). The panel uses this
 *  to render the honest "today's behavior — nothing quieted" state rather than a
 *  list of restrictions, so an idle/default HUD looks exactly like today. The
 *  category-count check (>= 7) keeps this robust to category-label drift. */
export function focusIsDefault(f: FocusActive): boolean {
  return (
    f.profile === "default" &&
    f.verbosity === "full" &&
    !f.suggestionsQuieted &&
    f.surfacing.length >= 7
  );
}

/** system / agent.active — router.rs (CONTRACT part A). Emitted when Jarvis-
 *  Prime delegates a request to an agent, and once per agent during a roll
 *  call. `hue` (0..360) drives the R3F core color + the constellation glow;
 *  `role` is the one-liner for the active-agent affordance. The HUD also has
 *  a static mirror (core/agents.ts) so `role`/`hue` can be missing on the
 *  event and still resolve for a known `name`. */
export interface AgentActiveData {
  name: string;
  role?: string;
  hue?: number;
}

/* ------------------------------------------------------------------------ *
 * Micro-app runtime relay (SANDBOX.md / build contract). jarvisd is the     *
 * ONLY socket-holder: each app talks to the daemon over its own per-app     *
 * Unix socket, and the daemon relays the data onto 7177 as `app.*` system   *
 * events so the HUD panel renders WITHOUT opening its own socket.           *
 * ------------------------------------------------------------------------ */

/** system / app.started — apps.rs, on a verified launch + socket connect. */
export interface AppStartedData {
  name: string;
}

/** system / app.stopped — apps.rs, on stop()/give-up. */
export interface AppStoppedData {
  name: string;
}

/** system / app.data — apps.rs relays each App->host "items"/"status" line.
 *  `topic` comes from the manifest `telemetry_topics` (or "feed"); `payload`
 *  is the verbatim app `data` object — opaque to the reducer beyond the
 *  per-topic shapes the panel knows how to render. */
export interface AppDataData {
  name: string;
  topic: string;
  payload: Record<string, unknown>;
}

/** system / app.op_forwarded — router.rs (handle_silicon_canvas), emitted when a
 *  voice command is translated into a structured op and forwarded to a running
 *  micro-app over its per-app socket (e.g. "show me the 3V3 net" -> Silicon
 *  Canvas `select.net`). `op` is the verbatim JSON op line that was sent. This
 *  is an activity/provenance signal (what JARVIS just did), not a panel surface
 *  — the reducer logs it to the actions ticker + a transient toast. */
export interface AppOpForwardedData {
  name: string;
  op: string;
}

/** global-scan "feed" payload — one polled item (apps/global-scan/main.py).
 *  `category` may flag breaking/alert items for the red accent. */
export interface GlobalScanItem {
  title: string;
  source: string;
  url: string;
  published: string; // iso8601 (or feed-native), may be empty
  category: string;
  summary: string;
}

/** global-scan "feed" payload (type=="items" line). */
export interface GlobalScanFeedPayload {
  brief: string;
  items: GlobalScanItem[];
  fetched_at: string; // iso8601
}

/* ------------------------------------------------------------------------ *
 * Silicon-Canvas micro-app payloads (apps/silicon-canvas — runtime=binary,  *
 * gpu=true, surface="fullscreen"). The Metal IOSurface compositing runs     *
 * ON-DEVICE; these are the telemetry payloads the HUD-side panel renders,   *
 * transcribed VERBATIM from the app's wire structs in                       *
 * apps/silicon-canvas/src/ops.rs (the Rust serde shapes are truth):         *
 *   RenderMs / Viewport / Selection (+ LayerVisibility, NetSelection,       *
 *   ComponentSelection, ErcMarker, Point). Relayed by the daemon as         *
 *   `app.data` with `topic` ∈ {canvas.render_ms, canvas.viewport,           *
 *   canvas.selection} (apps.rs::resolve_topic). Parsed defensively below —  *
 *   a malformed/partial payload yields null, never a throw.                 *
 * ------------------------------------------------------------------------ */

/** Manifest name of the silicon-canvas micro-app + its three declared topics
 *  (apps/silicon-canvas/manifest.toml `telemetry_topics`). */
export const CANVAS_TOPIC_RENDER_MS = "canvas.render_ms";
export const CANVAS_TOPIC_VIEWPORT = "canvas.viewport";
export const CANVAS_TOPIC_SELECTION = "canvas.selection";

/** canvas.render_ms — frame stats published at 1 Hz (SPEC §2 / ops.rs RenderMs).
 *  `p50`/`p95` are per-frame CPU+submit milliseconds over the last second;
 *  `draws` is the draw-call count last frame (target ≤ 30); `culled_pct` is the
 *  share of the scene dropped by viewport/LOD culling. */
export interface CanvasRenderMs {
  p50: number;
  p95: number;
  draws: number;
  culledPct: number;
}

/** One (layer name, visible) pair of canvas.viewport.layer_visibility — empty
 *  for a schematic (single logical layer); the PCB layer stack otherwise. */
export interface CanvasLayerVisibility {
  layer: string;
  visible: boolean;
}

/** canvas.viewport — camera pose on change, throttled 10 Hz (SPEC §3 /
 *  ops.rs Viewport). `x`/`y` is the view center in scene space (mm); `scale` is
 *  pixels-per-mm (the zoom). Drives the HUD minimap affordance. */
export interface CanvasViewport {
  x: number;
  y: number;
  scale: number;
  layerVisibility: CanvasLayerVisibility[];
}

/** The selected-net summary (SPEC §4 / ops.rs NetSelection:
 *  `{net, name, entity_count, pin_count}`). */
export interface CanvasNetSelection {
  net: number; // raw net id (index), for cross-probe correlation
  name: string; // human net name, e.g. "3V3"
  entityCount: number;
  pinCount: number;
}

/** The selected-component summary (SPEC §4 / ops.rs ComponentSelection). */
export interface CanvasComponentSelection {
  component: number;
  reference: string; // e.g. "U3"
  value: string; // e.g. "STM32F405"
  pinCount: number;
}

/** ERC marker severity (SPEC §5 / ops.rs ErcSeverity, lowercase on the wire).
 *  amber warning / red error — the ONLY two severities the app emits. */
export type CanvasErcSeverity = "warning" | "error";

/** One ERC finding (SPEC §5 / ops.rs ErcMarker: `{code, severity, at, message}`).
 *  `code` is a stable machine code (e.g. "unconnected_pin", "output_conflict");
 *  `at` is the fault coordinate in scene space; `message` is the panel-list text. */
export interface CanvasErcFinding {
  code: string;
  severity: CanvasErcSeverity;
  at: { x: number; y: number };
  message: string;
}

/** EntityRef kinds the trace front can land on (ops.rs ids.rs EntityKind,
 *  snake_case on the wire). Kept as a plain string on the parsed shape so an
 *  unknown future kind still surfaces rather than being dropped — the panel
 *  treats it opaquely (only "via" drives the cross-layer affordance via the
 *  separate `crossesLayer` flag). */
export type CanvasEntityKind =
  | "via"
  | "track"
  | "pad"
  | "wire"
  | "junction"
  | "label"
  | "zone"
  | "component"
  | "sheet";

/** One reference to an entity at the trace front (ops.rs EntityRef
 *  `{kind, index}`). `kind` is the raw snake_case string from the wire. */
export interface CanvasEntityRef {
  kind: string;
  index: number;
}

/** canvas.selection `trace` sub-payload (SPEC §4 / ops.rs TraceStep) — present
 *  ONLY while a net trace is walking (emitted by trace.start and each
 *  trace.step), ABSENT on plain net/component selections, ERC drops, and after
 *  trace.stop / select.net. The on-device GPU surface does the actual via-flash;
 *  this is the telemetry-driven progress + cross-layer affordance, not a render. */
export interface CanvasTraceStep {
  at: CanvasEntityRef; // entity now at the trace front
  distance: number; // BFS electrical distance from seed (seed = 0)
  crossesLayer: boolean; // true on a via / cross-copper-layer step
  step: number; // 1-based ordinal within the walk ("k" in "step k of n")
  of: number; // total nodes in the walk ("n")
  atEnd: boolean; // true on the last node (a further trace.step re-reports it)
}

/** canvas.selection — published on selection change AND once after ERC runs
 *  (SPEC §4/§5 / ops.rs Selection). All four are optional so one channel serves
 *  selection updates and the ERC result drop: a plain selection omits `erc`; an
 *  ERC drop omits `net`/`component`. `erc: []` means "ran clean"; absent `erc`
 *  means "not an ERC result line". `trace` is present only while tracing and is
 *  null otherwise. */
export interface CanvasSelection {
  net: CanvasNetSelection | null;
  component: CanvasComponentSelection | null;
  /** null = this line is not an ERC result; [] = ERC ran with no findings. */
  erc: CanvasErcFinding[] | null;
  /** null unless this line is a trace.start / trace.step front (SPEC §4). */
  trace: CanvasTraceStep | null;
}

/** Parse a canvas.render_ms payload. Returns null unless all four numeric stats
 *  are present and finite (a partial stats line is not rendered). */
export function parseCanvasRenderMs(
  data: Record<string, unknown>,
): CanvasRenderMs | null {
  const p50 = num(data, "p50");
  const p95 = num(data, "p95");
  const draws = num(data, "draws");
  const culledPct = num(data, "culled_pct");
  if (p50 === null || p95 === null || draws === null || culledPct === null) {
    return null;
  }
  return { p50, p95, draws, culledPct };
}

/** Parse a canvas.viewport payload. Returns null unless x/y/scale are finite
 *  numbers; `layer_visibility` is coerced item-by-item (non-conforming entries
 *  dropped, never throwing) and defaults to [] when absent (a schematic). */
export function parseCanvasViewport(
  data: Record<string, unknown>,
): CanvasViewport | null {
  const x = num(data, "x");
  const y = num(data, "y");
  const scale = num(data, "scale");
  if (x === null || y === null || scale === null) return null;
  const rawLayers = data["layer_visibility"];
  const layerVisibility: CanvasLayerVisibility[] = Array.isArray(rawLayers)
    ? rawLayers
        .filter(isPlainObject)
        .map((o) => ({ layer: str(o, "layer"), visible: bool(o, "visible") }))
        .filter(
          (l): l is CanvasLayerVisibility => l.layer !== null && l.visible !== null,
        )
    : [];
  return { x, y, scale, layerVisibility };
}

/** Coerce one untrusted ERC marker object into a finding, or null if it lacks a
 *  usable code/severity (severity must be exactly "warning"|"error" — anything
 *  else is dropped rather than rendered with an unknown badge). `at` defaults to
 *  the origin when missing/malformed so a marker without coords still lists. */
function coerceErcFinding(o: Record<string, unknown>): CanvasErcFinding | null {
  const code = str(o, "code");
  const severity = str(o, "severity");
  if (code === null || (severity !== "warning" && severity !== "error")) {
    return null;
  }
  const atObj = isPlainObject(o["at"]) ? (o["at"] as Record<string, unknown>) : {};
  return {
    code,
    severity,
    at: { x: num(atObj, "x") ?? 0, y: num(atObj, "y") ?? 0 },
    message: str(o, "message") ?? "",
  };
}

/** Coerce one untrusted `trace` object into a trace step, or null if it lacks a
 *  usable `at` ref (an entity `kind` string + finite `index`). The numeric
 *  progress fields default defensively — distance/step/of to 0 and the flags to
 *  false — so a partial-but-located front still surfaces rather than being
 *  dropped wholesale. Returns null (never throws) on anything non-conforming so a
 *  plain selection / ERC drop is unaffected. */
function coerceTraceStep(o: Record<string, unknown>): CanvasTraceStep | null {
  const atObj = o["at"];
  if (!isPlainObject(atObj)) return null;
  const kind = str(atObj, "kind");
  const index = num(atObj, "index");
  if (kind === null || index === null) return null;
  return {
    at: { kind, index },
    distance: num(o, "distance") ?? 0,
    crossesLayer: bool(o, "crosses_layer") ?? false,
    step: num(o, "step") ?? 0,
    of: num(o, "of") ?? 0,
    atEnd: bool(o, "at_end") ?? false,
  };
}

/** Parse a canvas.selection payload. Every field is narrowed independently so a
 *  pure selection line (no `erc`) and a pure ERC drop (no `net`/`component`)
 *  both apply what they carry. `erc` distinguishes absent (null — not an ERC
 *  result) from an empty list (ERC ran clean). `trace` is null unless this line
 *  carries a well-formed trace front. Never throws on junk. */
export function parseCanvasSelection(
  data: Record<string, unknown>,
): CanvasSelection {
  let net: CanvasNetSelection | null = null;
  const netObj = data["net"];
  if (isPlainObject(netObj)) {
    const name = str(netObj, "name");
    const netId = num(netObj, "net");
    if (name !== null && netId !== null) {
      net = {
        net: netId,
        name,
        entityCount: num(netObj, "entity_count") ?? 0,
        pinCount: num(netObj, "pin_count") ?? 0,
      };
    }
  }

  let component: CanvasComponentSelection | null = null;
  const compObj = data["component"];
  if (isPlainObject(compObj)) {
    const reference = str(compObj, "reference");
    const compId = num(compObj, "component");
    if (reference !== null && compId !== null) {
      component = {
        component: compId,
        reference,
        value: str(compObj, "value") ?? "",
        pinCount: num(compObj, "pin_count") ?? 0,
      };
    }
  }

  let erc: CanvasErcFinding[] | null = null;
  const rawErc = data["erc"];
  if (Array.isArray(rawErc)) {
    erc = rawErc
      .filter(isPlainObject)
      .map(coerceErcFinding)
      .filter((f): f is CanvasErcFinding => f !== null);
  }

  let trace: CanvasTraceStep | null = null;
  const traceObj = data["trace"];
  if (isPlainObject(traceObj)) {
    trace = coerceTraceStep(traceObj);
  }

  return { net, component, erc, trace };
}

/* ------------------------------------------------------------------------ *
 * Vision micro-app payloads (apps/vision — runtime=binary, surface="panel",  *
 * gpu=true, net_hosts=[]). DEFENSIVE-ONLY, ON-DEVICE ONLY: the actual camera /*
 * screen capture and Apple Vision/Core ML inference run ON-DEVICE behind a    *
 * macOS TCC consent gate (NOT grantable by SBPL); this HUD-side panel renders  *
 * ONLY the telemetry the app relays. The wire carries counts / boxes / labels  *
 * / timing — NEVER pixels, NEVER identity. No facial recognition, no identity  *
 * database, no person re-identification: humans are detected as generic        *
 * rectangles, animals/objects as generic kinds. Frames/detections NEVER leave  *
 * the device. Six relay topics, matching apps/vision/manifest.toml             *
 * `telemetry_topics` (daemon/src/apps.rs::resolve_topic, default =             *
 * vision.detections). vision.screen is the OCR screen-read readout (READ ON    *
 * REQUEST) — its recognized text is SENSITIVE + TRANSIENT (the daemon keeps it *
 * off lifelong memory / optimizer traces; the HUD renders it live only).       *
 * Parsed defensively — a malformed/partial payload yields null (or drops the   *
 * offending sub-item), never a throw.                                          *
 * ------------------------------------------------------------------------ */

/** Manifest name of the vision micro-app + its five declared topics
 *  (apps/vision/manifest.toml `telemetry_topics`). vision.detections is the
 *  DEFAULT relay topic (first declared). */
export const VISION_APP = "vision";
export const VISION_TOPIC_DETECTIONS = "vision.detections";
export const VISION_TOPIC_STATUS = "vision.status";
export const VISION_TOPIC_MOTION = "vision.motion";
export const VISION_TOPIC_PERF = "vision.perf";
export const VISION_TOPIC_ERROR = "vision.error";
export const VISION_TOPIC_SCREEN = "vision.screen";

/** Detection kinds the app emits — GENERIC presence/objecthood, never identity.
 *  "human" is a human-as-rectangle (VNDetectHumanRectanglesRequest), NOT a
 *  named person. Kept loose (plain string on the parsed shape) so an unknown
 *  future kind still surfaces rather than being dropped; the known set drives
 *  the by-kind summary + per-row accent. */
export type VisionKind = "human" | "animal" | "object" | "salientRegion" | "motion";

/** The capture source — one of the user's OWN inputs only. */
export type VisionSource = "camera" | "screen" | "file";

/** A normalized Vision bounding box (origin bottom-left, 0..1). Pixels are
 *  NEVER on the wire — only the box geometry is. */
export interface VisionBox {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** One per-frame detection (vision.detections `detections[]`). `box` is
 *  normalized Vision coords; `label` is a GENERIC class label (e.g. "cat",
 *  "keyboard", "human"), never an identity. */
export interface VisionDetection {
  kind: string; // raw wire string (known set: VisionKind), kept opaque if novel
  box: VisionBox;
  confidence: number; // 0..1
  label: string;
}

/** vision.detections — per-frame detection summary (DEFAULT topic). `byKind`
 *  carries only the kinds present in this frame (counts). `detections` is the
 *  per-box list. Counts/boxes/labels ONLY — no pixels, no identity. */
export interface VisionDetections {
  frame: number;
  ts: number; // seconds
  source: string; // VisionSource on the wire, kept opaque
  count: number;
  byKind: Record<string, number>;
  detections: VisionDetection[];
}

/** vision.status — watch lifecycle / capability snapshot. `camera_authorized` /
 *  `screen_authorized` mirror the on-device TCC grant (the REAL gate; null when
 *  the app did not report it). `state` is kept as a plain string so an unknown
 *  future lifecycle value still surfaces. */
export interface VisionStatus {
  state: string; // "idle" | "watching" | "analyzing" | "stopped"
  source: string | null;
  sensitivity: number | null; // 0..1
  cameraAuthorized: boolean | null;
  screenAuthorized: boolean | null;
  message: string | null;
}

/** vision.motion — a motion event (frame-to-frame change crossing threshold).
 *  Generic motion only — magnitude + region, never what moved. */
export interface VisionMotion {
  frame: number;
  ts: number;
  source: string;
  magnitude: number; // 0..1
  region: VisionBox;
}

/** vision.perf — inference timing snapshot. `fps` is the inference-bound
 *  ceiling (1000/p50), NOT a live capture/camera rate. `computeUnit` is the
 *  REQUESTED compute eligibility (ane/gpu/all) — Apple exposes no actual
 *  execution-unit residency, so it is the requested unit, never a measured one. */
export interface VisionPerf {
  p50Ms: number;
  p95Ms: number;
  fps: number;
  frames: number;
  computeUnit: string;
}

/** vision.error — a recoverable error. `code` is a stable machine code
 *  (e.g. "tcc_denied" | "bad_op" | "decode_failed"). `tcc_denied` is the
 *  honest, expected state when the user has not granted Camera / Screen
 *  Recording consent on-device. */
export interface VisionError {
  code: string;
  message: string;
  source: string | null;
}

/** A 2D point (normalized 0..1), the center of a recognized text block — used to
 *  LOCATE (not click) a control. */
export interface VisionPoint {
  x: number;
  y: number;
}

/** One recognized text block from a vision.screen OCR read. `text` is the
 *  recognized GLYPH string (DEFENSIVE: glyphs only — never a face/person id).
 *  `box`/`center` are normalized geometry (Vision bottom-left origin). `isControl`
 *  flags a short button-ish label the structuring marked as a candidate control;
 *  READ-ONLY — the HUD only describes/locates it, nothing clicks. */
export interface VisionScreenBlock {
  text: string;
  box: VisionBox;
  center: VisionPoint;
  confidence: number; // 0..1
  isControl: boolean;
}

/** vision.screen — a one-shot OCR screen-read readout (READ ON REQUEST). Carries
 *  the full readable `text` (reading order), the per-block list, the
 *  control-candidate subset, and — only for a "where is <X>" request — the
 *  `query` and the best-matching `located` block (with its match `score`).
 *
 *  PRIVACY: the recognized text is SENSITIVE (it can contain on-screen
 *  passwords/messages) and TRANSIENT — the daemon keeps it OFF lifelong memory
 *  and optimizer traces (router::is_screen_read gate). It surfaces ONLY here, in
 *  the live HUD readout, never persisted by the HUD either. */
export interface VisionScreen {
  frame: number;
  ts: number;
  source: string; // VisionSource on the wire, kept opaque
  blockCount: number;
  text: string;
  blocks: VisionScreenBlock[];
  controls: VisionScreenBlock[];
  query: string | null;
  located: (VisionScreenBlock & { score: number }) | null;
  /** Which read produced this readout — "screen" (on-screen OCR), "handwriting"
   *  (#28 handwriting/whiteboard recognizer), "document" (#29 camera document
   *  scanner), or "context" (#42 continuous screen-context snapshot — normally
   *  routed into the daemon's transient ring, not relayed; labeled honestly if it
   *  ever surfaces). Defaults to "screen" for older payloads that omit it. Lets the
   *  HUD LABEL the read honestly without a separate topic. */
  readKind: "screen" | "handwriting" | "document" | "context";
  /** NON-RAW-TEXT signal: whether ANY text was recognized this read. Lets a
   *  status-style readout say "read something / nothing" without rendering the
   *  sensitive glyphs. Defaults to whether `text` is non-empty. */
  textPresent: boolean;
  /** NON-RAW-TEXT signal: how MANY characters were recognized (length, NOT the
   *  glyphs). Lets the HUD show "read N chars" without exposing the content. */
  textLength: number;
  /** HONEST document-detected bool for the #29 scanner (null for screen/
   *  handwriting reads, where document detection is N/A). When false, the scanner
   *  found NO page and the readout is honestly empty — never a fabricated page. */
  documentDetected: boolean | null;
}

/** Coerce one untrusted box object into a VisionBox, defaulting any missing /
 *  non-finite component to 0 (a partial box still places at the origin rather
 *  than dropping the detection). Never throws. */
function coerceVisionBox(v: unknown): VisionBox {
  const o = isPlainObject(v) ? v : {};
  return {
    x: num(o, "x") ?? 0,
    y: num(o, "y") ?? 0,
    w: num(o, "w") ?? 0,
    h: num(o, "h") ?? 0,
  };
}

/** Coerce one untrusted detection object into a VisionDetection, or null if it
 *  lacks a usable `kind` string. `box` defaults to the origin-rect when
 *  missing/malformed; `confidence` defaults to 0; `label` to "". Never throws. */
function coerceVisionDetection(o: Record<string, unknown>): VisionDetection | null {
  const kind = str(o, "kind");
  if (kind === null) return null;
  return {
    kind,
    box: coerceVisionBox(o["box"]),
    confidence: num(o, "confidence") ?? 0,
    label: str(o, "label") ?? "",
  };
}

/** Parse a vision.detections payload (DEFAULT topic). Returns null unless
 *  `frame` is a finite number (the one structural anchor). `by_kind` is coerced
 *  entry-by-entry (only finite numeric counts kept); `detections` is coerced
 *  item-by-item (non-conforming entries dropped). Counts/boxes/labels ONLY —
 *  never pixels, never identity. Never throws on junk. */
export function parseVisionDetections(
  data: Record<string, unknown>,
): VisionDetections | null {
  const frame = num(data, "frame");
  if (frame === null) return null;

  const rawByKind = data["by_kind"];
  const byKind: Record<string, number> = {};
  if (isPlainObject(rawByKind)) {
    for (const [k, v] of Object.entries(rawByKind)) {
      if (typeof v === "number" && Number.isFinite(v)) byKind[k] = v;
    }
  }

  const rawDet = data["detections"];
  const detections: VisionDetection[] = Array.isArray(rawDet)
    ? rawDet
        .filter(isPlainObject)
        .map(coerceVisionDetection)
        .filter((d): d is VisionDetection => d !== null)
    : [];

  return {
    frame,
    ts: num(data, "ts") ?? 0,
    source: str(data, "source") ?? "",
    count: num(data, "count") ?? detections.length,
    byKind,
    detections,
  };
}

/** Parse a vision.status payload. Returns null unless `state` is a non-empty
 *  string (the lifecycle anchor). Optional fields stay null when omitted so the
 *  panel can distinguish "not reported" from a value — notably the TCC flags,
 *  which must read honestly as "unknown" rather than a fake "authorized".
 *  Never throws. */
export function parseVisionStatus(data: Record<string, unknown>): VisionStatus | null {
  const state = str(data, "state");
  if (state === null || state.length === 0) return null;
  return {
    state,
    source: str(data, "source"),
    sensitivity: num(data, "sensitivity"),
    cameraAuthorized: bool(data, "camera_authorized"),
    screenAuthorized: bool(data, "screen_authorized"),
    message: str(data, "message"),
  };
}

/** Parse a vision.motion payload. Returns null unless `frame` is finite. The
 *  numeric/region fields default defensively so a partial event still surfaces
 *  as generic motion (magnitude + region — never what moved). Never throws. */
export function parseVisionMotion(data: Record<string, unknown>): VisionMotion | null {
  const frame = num(data, "frame");
  if (frame === null) return null;
  return {
    frame,
    ts: num(data, "ts") ?? 0,
    source: str(data, "source") ?? "",
    magnitude: num(data, "magnitude") ?? 0,
    region: coerceVisionBox(data["region"]),
  };
}

/** Parse a vision.perf payload. Returns null unless all four numeric stats are
 *  finite; `compute_unit` defaults to "" when absent. A partial stats line is
 *  not rendered (mirrors parseCanvasRenderMs). Never throws. */
export function parseVisionPerf(data: Record<string, unknown>): VisionPerf | null {
  const p50Ms = num(data, "p50_ms");
  const p95Ms = num(data, "p95_ms");
  const fps = num(data, "fps");
  const frames = num(data, "frames");
  if (p50Ms === null || p95Ms === null || fps === null || frames === null) {
    return null;
  }
  return { p50Ms, p95Ms, fps, frames, computeUnit: str(data, "compute_unit") ?? "" };
}

/** Parse a vision.error payload. Returns null unless `code` is a non-empty
 *  string (the machine code anchor); `message` defaults to "" and `source`
 *  stays null when omitted. Never throws. */
export function parseVisionError(data: Record<string, unknown>): VisionError | null {
  const code = str(data, "code");
  if (code === null || code.length === 0) return null;
  return {
    code,
    message: str(data, "message") ?? "",
    source: str(data, "source"),
  };
}

/** Coerce one untrusted center-point into a VisionPoint, defaulting missing /
 *  non-finite components to 0. Never throws. */
function coerceVisionPoint(v: unknown): VisionPoint {
  const o = isPlainObject(v) ? v : {};
  return { x: num(o, "x") ?? 0, y: num(o, "y") ?? 0 };
}

/** Coerce one untrusted screen block into a VisionScreenBlock, or null when it
 *  lacks a usable `text` glyph string (the structural anchor — an OCR block with
 *  no text is meaningless). `box`/`center` default to the origin; `confidence`
 *  to 0; `is_control` to false. DEFENSIVE: `text` is a glyph string only — never
 *  an identity. Never throws. */
function coerceVisionScreenBlock(o: Record<string, unknown>): VisionScreenBlock | null {
  const text = str(o, "text");
  if (text === null) return null;
  return {
    text,
    box: coerceVisionBox(o["box"]),
    center: coerceVisionPoint(o["center"]),
    confidence: num(o, "confidence") ?? 0,
    isControl: bool(o, "is_control") ?? false,
  };
}

/** Parse a vision.screen OCR screen-read payload. Returns null unless `frame` is
 *  a finite number (the structural anchor). `blocks` / `controls` are coerced
 *  item-by-item (non-conforming entries dropped). `query` stays null unless a
 *  where-is request set it; `located` stays null unless the app returned a best
 *  match (carrying its `score`). Glyph text ONLY — never pixels, never identity.
 *
 *  PRIVACY: the parsed `text` is SENSITIVE + TRANSIENT — the daemon keeps it off
 *  lifelong memory / optimizer traces (router::is_screen_read), and this parser
 *  is a render-only narrowing (the HUD never persists it either). Never throws. */
export function parseVisionScreen(data: Record<string, unknown>): VisionScreen | null {
  const frame = num(data, "frame");
  if (frame === null) return null;

  const rawBlocks = data["blocks"];
  const blocks: VisionScreenBlock[] = Array.isArray(rawBlocks)
    ? rawBlocks
        .filter(isPlainObject)
        .map(coerceVisionScreenBlock)
        .filter((b): b is VisionScreenBlock => b !== null)
    : [];

  const rawControls = data["controls"];
  const controls: VisionScreenBlock[] = Array.isArray(rawControls)
    ? rawControls
        .filter(isPlainObject)
        .map(coerceVisionScreenBlock)
        .filter((b): b is VisionScreenBlock => b !== null)
    : [];

  // `located` only when the app returned a best-matching block (a where-is
  // request). It is a block shape plus a numeric `score`; a partial/absent
  // located stays null rather than rendering a phantom hit.
  let located: (VisionScreenBlock & { score: number }) | null = null;
  if (isPlainObject(data["located"])) {
    const lb = coerceVisionScreenBlock(data["located"]);
    if (lb !== null) {
      located = { ...lb, score: num(data["located"], "score") ?? 0 };
    }
  }

  const text = str(data, "text") ?? "";
  // read_kind defaults to "screen" for older payloads; an unknown value also
  // falls back to "screen" so the readout never renders an undefined kind.
  const rawKind = str(data, "read_kind");
  const readKind: "screen" | "handwriting" | "document" | "context" =
    rawKind === "handwriting" || rawKind === "document" || rawKind === "context"
      ? rawKind
      : "screen";
  // document_detected is honored only for the document scanner (it is omitted on
  // the wire for screen/handwriting reads, where it is N/A -> null).
  const documentDetected = readKind === "document" ? bool(data, "document_detected") : null;

  return {
    frame,
    ts: num(data, "ts") ?? 0,
    source: str(data, "source") ?? "",
    blockCount: num(data, "block_count") ?? blocks.length,
    text,
    blocks,
    controls,
    query: str(data, "query"),
    located,
    readKind,
    // text_present / text_length are the NON-RAW-TEXT signal; default from the
    // recognized text when the wire omits them (older payloads).
    textPresent: bool(data, "text_present") ?? text.length > 0,
    textLength: num(data, "text_length") ?? text.length,
    documentDetected,
  };
}

/* ------------------------------------------------------------------------ *
 * vision.describe — the ON-DEVICE VISION-LANGUAGE-MODEL (VLM) describe event.  *
 * DISTINCT from vision.screen (OCR): OCR reads the TEXT GLYPHS on screen, the  *
 * VLM DESCRIBES + reasons about the visual SCENE (an mlx-vlm Qwen2-VL-class    *
 * model running entirely on-device — the image's pixels go ONLY to that model  *
 * and NEVER leave the device, never to the cloud). It is DEVICE-GATED: it      *
 * needs a multi-GB VLM model download + enough RAM, so it ships OFF/opt-in.    *
 *                                                                              *
 * The event rides channel "local", event "vision.describe", payload           *
 * {"source": "screen"|"image", "available": bool, "vlm": bool}. CRITICAL — the *
 * event carries NO PIXELS, NO DESCRIPTION TEXT, and NO PATH: the visual        *
 * content (the most sensitive thing in this op) NEVER rides telemetry, so this *
 * readout surfaces ONLY the source kind + whether the on-device VLM actually   *
 * produced a description (`available`) + whether the model is enabled (`vlm`,  *
 * mirroring cfg.vision.enabled). The description itself is persona-voiced over *
 * the SPOKEN reply and kept TRANSIENT (never seeded into lifelong memory) — it *
 * is never persisted here either. `available` is true ONLY when the VLM ran    *
 * and described; it is false on EVERY gate / confine / unavailable / transport *
 * fall-back (where the daemon falls back honestly, e.g. to OCR). Parsed        *
 * defensively — a malformed/partial payload yields null, never a throw.        *
 * ------------------------------------------------------------------------ */

/** Channel-"local" event name the daemon emits after a describe turn
 *  (daemon/src/router.rs::handle_describe). */
export const VISION_DESCRIBE_EVENT = "vision.describe";

/** The source kinds the daemon describes — a screen frame or a user-named image
 *  file (PATH-CONFINED). Kept loose on the parsed shape so an unknown future
 *  source still surfaces rather than being dropped. */
export type VisionDescribeSource = "screen" | "image";

/** vision.describe — the metadata-only outcome of one on-device VLM describe.
 *  Carries NO pixels / NO description text / NO path (those NEVER ride
 *  telemetry). `available` is true ONLY when the on-device VLM produced a
 *  description; false on every gate/confine/unavailable/transport fall-back.
 *  `vlm` mirrors cfg.vision.enabled (the OFF/opt-in model flag). */
export interface VisionDescribe {
  source: string; // "screen" | "image" on the wire, kept opaque if novel
  available: boolean;
  vlm: boolean;
}

/** Parse a vision.describe payload. Returns null unless `source` is a non-empty
 *  string (the structural anchor); `available`/`vlm` default to false when
 *  omitted (an unknown availability reads as NOT available — never a fake
 *  "it described"). NO pixels / text / path are present to parse — the visual
 *  content never crosses the wire. Never throws. */
export function parseVisionDescribe(data: Record<string, unknown>): VisionDescribe | null {
  const source = str(data, "source");
  if (source === null || source.length === 0) return null;
  return {
    source,
    available: bool(data, "available") ?? false,
    vlm: bool(data, "vlm") ?? false,
  };
}

/* ------------------------------------------------------------------------ *
 * image.generated — the ON-DEVICE IMAGE-GENERATION readout (task #18, channel  *
 * "local"). The daemon runs a Stable-Diffusion-class MLX DIFFUSION model       *
 * ON-DEVICE in response to a "generate/make/draw an image of X" intent and      *
 * emits this HUD-bound event (NEVER over the network) after the generate_image  *
 * op returns. The model + RAM are a HARD device gate: a multi-GB diffusion      *
 * model has to be downloaded + enabled, and generation is slow on smaller        *
 * chips, so it SHIPS OFF/opt-in ([image].enabled + the model id, pinned OFF).   *
 *                                                                              *
 * The event rides channel "local", event "image.generated", payload            *
 * {"available": bool, "path": <saved on-device abs path under state/images/ |   *
 *  null>, "model": <image model id | null>, "size": <int | null>,              *
 *  "steps": <int | null>, "image": <cfg.image.enabled bool>}. CRITICAL — the    *
 * event NEVER carries the PROMPT and NEVER any PIXELS: the two most sensitive   *
 * things in the op (what the user asked for + the image itself) NEVER ride       *
 * telemetry. The diffusion SEED is intentionally DROPPED too (not surfaced, not  *
 * spoken, not forwarded). So this readout surfaces ONLY: whether the on-device  *
 * model actually produced an image (`available`), WHERE on the device the image  *
 * landed (`path`, a local abs path under state/images/), and non-secret model/  *
 * size/steps metadata, plus whether the model is enabled (`image`, mirroring     *
 * cfg.image.enabled). `available` is true ONLY when the on-device diffusion      *
 * model actually generated + saved an image; it is false on EVERY gate /         *
 * unavailable / transport fall-back — and there is NEVER a fabricated image and  *
 * NEVER a silent cloud fall-back (image gen is LOCAL only). On a transport       *
 * error a SEPARATE `inference.unavailable` {"op":"generate_image",...} event is  *
 * emitted (system source). Parsed defensively — a malformed payload yields null, *
 * never a throw.                                                                *
 * ------------------------------------------------------------------------ */

/** Channel-"local" event name the daemon emits after a generate-image turn
 *  (daemon/src/router.rs image intent -> infer.generate_image). HUD-bound only;
 *  it NEVER rides the network. */
export const IMAGE_GENERATED_EVENT = "image.generated";

/** image.generated — the metadata-only outcome of one on-device diffusion run.
 *  Carries NO prompt / NO pixels (those NEVER ride telemetry) and DROPS the
 *  seed. `available` is true ONLY when the on-device model produced + saved an
 *  image; false on every gate/unavailable/transport fall-back (NEVER a fake
 *  image, NEVER a cloud fall-back). `path` is the saved on-device ABS path under
 *  state/images/ (null when unavailable). `model`/`size`/`steps` are non-secret
 *  metadata (null when not reported). `image` mirrors cfg.image.enabled (the
 *  OFF/opt-in model flag). */
export interface ImageGenerated {
  available: boolean;
  /** The saved on-device abs path under state/images/, or null when no image was
   *  produced. The PIXELS never ride the wire — only where the file landed. */
  path: string | null;
  /** The image model id (non-secret metadata), or null when not reported. */
  model: string | null;
  /** The generated size (px), or null when not reported. */
  size: number | null;
  /** The diffusion step count, or null when not reported. */
  steps: number | null;
  /** cfg.image.enabled — the OFF/opt-in model flag, surfaced so the panel's
   *  ships-OFF copy is grounded in the payload, not a hard-code. */
  image: boolean;
}

/** Parse an image.generated payload. Returns null only when the payload is not a
 *  plain object (the one structural guard) — every field is otherwise narrowed
 *  defensively. `available`/`image` default to FALSE when omitted (an unknown
 *  availability reads as NOT available + the model as OFF — never a fake "it
 *  generated"). `path` is kept ONLY when available is true AND a non-empty string
 *  path is present: an unavailable outcome NEVER carries a path (no phantom
 *  file), and an "available but no path" payload is downgraded to NOT available
 *  (an image with nowhere on-device to point at is not a real result). The
 *  PROMPT, PIXELS and SEED are never present to parse — they never cross the
 *  wire. NEVER throws. */
export function parseImageGenerated(data: Record<string, unknown>): ImageGenerated | null {
  if (!isPlainObject(data)) return null;
  const image = bool(data, "image") ?? false;
  const rawAvailable = bool(data, "available") ?? false;
  const path = str(data, "path");
  const hasPath = path !== null && path.length > 0;
  // An image must have a real on-device path to be a usable result; an
  // "available with no path" payload is downgraded to NOT available (honest:
  // there is no file to surface), never rendered as a phantom success.
  const available = rawAvailable && hasPath;
  return {
    available,
    path: available && hasPath ? path : null,
    model: str(data, "model"),
    size: num(data, "size"),
    steps: num(data, "steps"),
    image,
  };
}

/* ------------------------------------------------------------------------ *
 * vision.sound — the Apple SOUND ANALYSIS class readout (task #15). The AUDIO  *
 * analog of vision.detections: the Vision app runs the BUILT-IN Sound Analysis *
 * classifier (SNClassifierIdentifier.version1 — a FIXED ~300-class vocabulary, *
 * NOT "any sound") on-device/ANE-eligible over a supplied audio CLIP (a wav/   *
 * buffer the daemon wrote from its OWN captured audio) and emits the top sound *
 * classes {label, confidence}. This rides the `app.data` relay (topic          *
 * "vision.sound"), ASYNCHRONOUSLY after the one-shot "what was that sound"      *
 * intent forwards the classify.sound op (which itself emits the labels-only     *
 * `audio.sound` proof telemetry).                                              *
 *                                                                              *
 * PRIVACY (mirrors the Swift VisionEvent contract verbatim): each classes[]    *
 * entry carries ONLY a label + a 0..1 confidence — there is deliberately NO    *
 * audio field, NO clip samples, NO path. The audio NEVER leaves the device;    *
 * only the derived sound-class LABELS cross the socket. This is audio SCENE     *
 * understanding (dog bark / doorbell / alarm / music), DISTINCT from STT        *
 * (speech) — no transcript is produced. An empty / too-short / undecodable     *
 * clip yields the op's honest `no_sound_classes` vision.error instead (the      *
 * daemon/app never invents a class). Parsed defensively — a malformed payload   *
 * yields null, junk classes are dropped, never a throw.                        *
 * ------------------------------------------------------------------------ */

/** The relay topic the Vision app emits sound-class readouts on (matches
 *  apps/vision/manifest.toml `telemetry_topics` + VisionTopic.sound). */
export const VISION_TOPIC_SOUND = "vision.sound";

/** One classified sound class from a vision.sound readout. LABELS ONLY: a
 *  generic class `label` from the FIXED ~300-class built-in classifier (e.g.
 *  "dog_bark", "doorbell", "music") plus its `confidence` (0..1). There is no
 *  audio here — the sound itself never rides the wire. */
export interface VisionSoundClass {
  label: string;
  confidence: number; // 0..1
}

/** vision.sound — a top-sound-classes readout from one on-device Sound Analysis
 *  pass over a supplied clip. `classes` is the ranked label list; `classifier`
 *  is the built-in vocabulary tag (so the panel can state the fixed ~300-class
 *  vocabulary honestly, not "any sound"); `computeUnit` is the requested compute
 *  eligibility (ane/gpu/all), mirroring vision.perf. Carries NO audio. */
export interface VisionSound {
  ts: number; // seconds
  source: string; // "sound" on the wire, kept opaque if novel
  count: number;
  classes: VisionSoundClass[];
  classifier: string;
  computeUnit: string;
}

/** Coerce one untrusted sound-class object into a VisionSoundClass, or null when
 *  it lacks a usable `label` string (the structural anchor — an unlabeled class
 *  is meaningless). `confidence` defaults to 0 when missing/non-finite. DEFENSIVE:
 *  ONLY label + confidence survive — any smuggled audio/path/sample field is
 *  ignored (it never reaches this shape). Never throws. */
function coerceVisionSoundClass(o: Record<string, unknown>): VisionSoundClass | null {
  const label = str(o, "label");
  if (label === null) return null;
  return { label, confidence: num(o, "confidence") ?? 0 };
}

/** Parse a vision.sound payload. Returns null unless `classes` is an array (the
 *  structural anchor — a readout with no classes array is not a render frame; an
 *  EMPTY array is a valid "ran, nothing above the floor" frame). Classes are
 *  coerced item-by-item (non-conforming entries dropped). `count` defaults to the
 *  kept-classes length; `classifier`/`source`/`compute_unit` default to "".
 *  LABELS ONLY — the audio never crosses the wire, so there is nothing audio-like
 *  to parse. Never throws on junk. */
export function parseVisionSound(data: Record<string, unknown>): VisionSound | null {
  const rawClasses = data["classes"];
  if (!Array.isArray(rawClasses)) return null;
  const classes: VisionSoundClass[] = rawClasses
    .filter(isPlainObject)
    .map(coerceVisionSoundClass)
    .filter((c): c is VisionSoundClass => c !== null);
  return {
    ts: num(data, "ts") ?? 0,
    source: str(data, "source") ?? "",
    count: num(data, "count") ?? classes.length,
    classes,
    classifier: str(data, "classifier") ?? "",
    computeUnit: str(data, "compute_unit") ?? "",
  };
}

/* ------------------------------------------------------------------------ *
 * audio.sound_monitor — the OPT-IN ambient sound-monitor STATE indicator       *
 * (task #15, channel "local"). Emitted once at daemon startup from              *
 * [audio].sound_monitor (which SHIPS OFF + is pinned). Payload                  *
 * {"enabled": bool, "consent": "device_gated", "labels_only": true,            *
 *  "audio_left_device": false}.                                                *
 *                                                                              *
 * `enabled` drives the HUD's MONITORING / OFF indicator. It is the operator's  *
 * config opt-in — there is NO tool/agent/model route that can flip it, and no  *
 * default-on / auto-arm anywhere. EVEN when enabled, the actual continuous      *
 * ambient capture is DEVICE-GATED behind macOS mic/TCC consent the daemon       *
 * cannot grant; `consent: "device_gated"` states that honestly. PRIVACY: only   *
 * sound-class LABELS would ever be emitted (labels_only) — the audio never      *
 * leaves the device (audio_left_device=false). Parsed defensively into an       *
 * honest fail-OFF snapshot — a malformed payload reads as NOT enabled, never a  *
 * fake "monitoring". Never throws. */
export const AUDIO_SOUND_MONITOR_EVENT = "audio.sound_monitor";

/** Parsed audio.sound_monitor state. `enabled` is the opt-in monitor switch
 *  (false = the shipped default, the monitor never started). `consent` is the
 *  honest consent posture ("device_gated" — macOS mic/TCC is a separate gate).
 *  `labelsOnly`/`audioLeftDevice` are the privacy invariants surfaced from the
 *  wire so the indicator copy is grounded in the payload, not a hard-code. */
export interface AudioSoundMonitor {
  enabled: boolean;
  consent: string;
  labelsOnly: boolean;
  audioLeftDevice: boolean;
}

/** Parse an audio.sound_monitor payload into an honest, fail-OFF snapshot. This
 *  NEVER returns null: a malformed/partial payload yields {enabled:false,
 *  consent:"device_gated", labelsOnly:true, audioLeftDevice:false} rather than a
 *  stale one, so the indicator always reads the current honest posture and a
 *  garbled payload can never fake "monitoring". `enabled` defaults to FALSE (the
 *  monitor never silently arms). Never throws. */
export function parseAudioSoundMonitor(data: Record<string, unknown>): AudioSoundMonitor {
  return {
    enabled: bool(data, "enabled") ?? false,
    consent: str(data, "consent") ?? "device_gated",
    labelsOnly: bool(data, "labels_only") ?? true,
    audioLeftDevice: bool(data, "audio_left_device") ?? false,
  };
}

/* ------------------------------------------------------------------------ *
 * screen_context.* — CONTINUOUS SCREEN CONTEXT (#42). The MOST privacy-       *
 * sensitive READ feature, so the wire is the tightest: it carries ONLY        *
 * whether the continuous capture loop is ACTIVE + the BOUNDED counts (held /  *
 * cap) — NEVER the recognized glyphs, NEVER the recalled redacted text. The   *
 * recognized text lives ONLY in the daemon's bounded, redacted, TRANSIENT     *
 * in-RAM ring; it is rendered into the persona-voiced recall reply and kept   *
 * off lifelong memory / optimizer traces — it never crosses this socket.      *
 *                                                                             *
 * Three envelopes, all source="system" / SECRET-FREE:                         *
 *  - screen_context.watching  {watching, ingested, held, cap} — emitted on    *
 *    each continuous snapshot. `watching` is the loop-active bit (the HUD's    *
 *    PROMINENT WATCHING indicator); `held`/`cap` are the bounded ring counts   *
 *    (held N / cap M); `ingested` is whether THIS snapshot fed the ring        *
 *    (false when the loop is OFF — the OFF-default gate is honest).            *
 *  - screen_context.configured {enabled, cap, interval_secs} — emitted once    *
 *    at startup. `enabled` SHIPS FALSE (the loop never runs by default).       *
 *  - screen_context.command {verb, enabled} — a recall/forget VOICE command    *
 *    just ran. `verb` is "recall" | "forget" ONLY (never the recalled text).   *
 *                                                                             *
 * The Swift capture loop ALSO frames its active window with a vision.status    *
 * {state:"watching", message:"screen_context.watching"} envelope (and an       *
 * honest "screen_context.watching=false" exit), which the existing            *
 * parseVisionStatus already covers — this surface reads the dedicated system   *
 * envelopes for the bounded counts. Parsed DEFENSIVELY: a malformed payload    *
 * fails OFF (watching:false / enabled:false), never a fake "watching"; junk    *
 * counts floor to 0. NEVER throws.                                            *
 * ------------------------------------------------------------------------ */

/** System event names the daemon emits for #42. */
export const SCREEN_CONTEXT_WATCHING_EVENT = "screen_context.watching";
export const SCREEN_CONTEXT_CONFIGURED_EVENT = "screen_context.configured";
export const SCREEN_CONTEXT_COMMAND_EVENT = "screen_context.command";

/** The fused, render-ready SCREEN-CONTEXT posture the HUD folds from the three
 *  screen_context.* envelopes. SECRET-FREE by construction: it holds ONLY the
 *  loop-active bit, the bounded ring counts, the startup config bounds, and the
 *  last command verb — NEVER the recognized glyphs or the recalled redacted
 *  text (those live only in the daemon's transient ring, never on this wire).
 *
 *  PRIVACY (held verbatim in the panel copy): OFF by default (`enabled` ships
 *  false); the live capture is TCC-DEVICE-GATED (Screen Recording, not
 *  SBPL-grantable); the ring is TRANSIENT (off lifelong memory / optimizer);
 *  glyph-only (NEVER a face / person id / embedding; pixels never leave the
 *  device); BOUNDED (an evict-oldest ring, `held` <= `cap`); FORGETTABLE
 *  ("forget my screen context" wipes it); READ-ONLY (recall describes, never
 *  actuates). */
export interface ScreenContext {
  /** Operator config opt-in (screen_context.configured.enabled). SHIPS FALSE —
   *  with it off the continuous loop never runs and the ring never grows. */
  enabled: boolean;
  /** The continuous capture LOOP is ACTIVE this moment (watching.watching). The
   *  PROMINENT amber WATCHING indicator. False/absent => the loop is not running
   *  (the OFF-default resting state). */
  watching: boolean;
  /** How many redacted entries the bounded ring currently holds (>= 0). NEVER
   *  the glyphs — only the count. */
  held: number;
  /** The ring's hard cap (evict-oldest past it). `held` <= `cap` always. */
  cap: number;
  /** Whether the MOST RECENT continuous snapshot was ingested into the ring
   *  (false when the loop is OFF — honest about the OFF-default gate). */
  ingested: boolean;
  /** Configured snapshot interval in seconds (screen_context.configured), or
   *  null until the startup config arrives. */
  intervalSecs: number | null;
  /** The last recall/forget command verb ("recall" | "forget"), or null until a
   *  command runs. NEVER the recalled redacted text. */
  lastVerb: string | null;
}

/** The honest resting state before any screen_context.* envelope arrives — the
 *  OFF-default the feature ships at: not enabled, not watching, an empty bounded
 *  ring, no command yet. */
export function screenContextInitial(): ScreenContext {
  return {
    enabled: false,
    watching: false,
    held: 0,
    cap: 0,
    ingested: false,
    intervalSecs: null,
    lastVerb: null,
  };
}

/** Fold a screen_context.watching payload into the prior posture. SECRET-FREE:
 *  reads ONLY `watching` (loop-active) + the bounded `held`/`cap` counts +
 *  `ingested`. Fails OFF — a malformed/absent `watching` reads as NOT watching
 *  (never a fake "watching"); junk counts floor to the prior value then 0. The
 *  config-derived `enabled`/`intervalSecs` + `lastVerb` are preserved from the
 *  prior posture (they ride the other two envelopes). Never throws. */
export function applyScreenContextWatching(
  prev: ScreenContext,
  data: Record<string, unknown>,
): ScreenContext {
  return {
    ...prev,
    watching: bool(data, "watching") ?? false,
    held: Math.max(0, num(data, "held") ?? prev.held),
    cap: Math.max(0, num(data, "cap") ?? prev.cap),
    ingested: bool(data, "ingested") ?? false,
  };
}

/** Fold the startup screen_context.configured payload into the posture. Reads
 *  the operator `enabled` opt-in (defaults FALSE — the loop never silently
 *  arms), the hard `cap`, and the `interval_secs`. The live `watching`/`held`
 *  counts are preserved (they ride the watching envelope). Never throws. */
export function applyScreenContextConfigured(
  prev: ScreenContext,
  data: Record<string, unknown>,
): ScreenContext {
  return {
    ...prev,
    enabled: bool(data, "enabled") ?? false,
    cap: Math.max(0, num(data, "cap") ?? prev.cap),
    intervalSecs: num(data, "interval_secs"),
  };
}

/** Fold a screen_context.command (recall/forget) into the posture. Reads ONLY
 *  the `verb` (recall | forget — never the recalled redacted text) + the
 *  `enabled` opt-in echoed back. A malformed/absent verb leaves `lastVerb`
 *  untouched (no fake command). Never throws. */
export function applyScreenContextCommand(
  prev: ScreenContext,
  data: Record<string, unknown>,
): ScreenContext {
  const verb = str(data, "verb");
  return {
    ...prev,
    enabled: bool(data, "enabled") ?? prev.enabled,
    lastVerb: verb !== null && verb.length > 0 ? verb : prev.lastVerb,
  };
}

/* ------------------------------------------------------------------------ *
 * Nexus micro-app payloads (apps/nexus — runtime="python" control plane      *
 * hosting a Rust cdylib realtime/DSP core via ctypes; audio=true, offline).   *
 * DEVICE-GATED, ON-DEVICE ONLY: the CoreAudio IOProc, aggregate-device, the    *
 * sub-10ms monitor RTT, AUv3 hosting, and EVERY live number below come from    *
 * the realtime core running against real audio hardware — they CANNOT be       *
 * produced headlessly. This HUD-side panel is the telemetry READOUT, never a   *
 * synthesizer: `measured_rtt_ms`, the per-channel meters, the LUFS readout,    *
 * the spectrum bands and clip events populate ONLY when the on-device audio    *
 * core runs; until then the panel shows its OFFLINE placeholder. Nexus ships   *
 * NO UI code — all rendering is HUD-side from these payloads (SPEC §6).        *
 *                                                                              *
 * Five relay topics, matching apps/nexus/manifest.toml `telemetry_topics`      *
 * (daemon/src/apps.rs::resolve_topic, default = audio.levels — the first       *
 * declared). Parsed DEFENSIVELY — a malformed/partial payload yields null (or  *
 * drops the offending sub-item), never a throw.                                *
 * ------------------------------------------------------------------------ */

/** Manifest name of the nexus micro-app + its five declared topics
 *  (apps/nexus/manifest.toml `telemetry_topics`). audio.levels is the DEFAULT
 *  relay topic (first declared). */
export const NEXUS_APP = "nexus";
export const NEXUS_TOPIC_LEVELS = "audio.levels";
export const NEXUS_TOPIC_ROUTES = "audio.routes";
export const NEXUS_TOPIC_GAIN = "audio.gain";
export const NEXUS_TOPIC_CLIPPING = "audio.clipping";
export const NEXUS_TOPIC_SPECTRUM = "audio.spectrum";

/** The fixed number of log-spaced spectrum bands (SPEC §6: 2048-pt FFT folded
 *  to 96 log bands, dBFS). A spectrum payload whose `bands` is not exactly this
 *  length is rejected (a partial FFT frame is not rendered). */
export const NEXUS_SPECTRUM_BANDS = 96;

/** VIEW cap on the routing-matrix dimensions the HUD will render (the daemon's
 *  real interfaces are tiny — DEFAULT 4x4, well under any audio device's channel
 *  count). The grid is drawn as `inputs x outputs` cells, so a malformed/spoofed
 *  audio.routes frame with a huge `inputs`/`outputs` would otherwise feed an
 *  enormous (or, past 2^32, a throwing) `Array.from({length})` straight into the
 *  panel render — a render-time freeze/OOM or RangeError that takes down the HUD.
 *  The dimensions are floored to non-negative integers and clamped here (mirrors
 *  CHART_SERIES_CAP / CHART_POINTS_CAP), so the parser is fail-safe by
 *  construction. 256 is far above any real interface. */
export const NEXUS_MATRIX_DIM_CAP = 256;

/** One channel's meter pair (audio.levels `ch[]`). `peakDbfs`/`rmsDbfs` are
 *  dBFS (<= 0; -inf floored on the wire as a large negative). These are LIVE
 *  meter taps from the realtime core — present only on-device. */
export interface NexusChannelLevel {
  peakDbfs: number;
  rmsDbfs: number;
}

/** audio.levels — per-channel peak/RMS plus the three BS.1770-4 LUFS readouts
 *  (SPEC §6, 30 Hz). `lufsM`/`lufsS` are momentary (400 ms) / short-term (3 s);
 *  `lufsI` is integrated (gated, BS.1770-4). All are LIVE on-device numbers. */
export interface NexusLevels {
  ch: NexusChannelLevel[];
  lufsM: number | null;
  lufsS: number | null;
  lufsI: number | null;
}

/** One above-floor crosspoint of the routing matrix (audio.routes `matrix[]`).
 *  `in`/`out` are channel indices; `gainDb` is the crosspoint gain (-inf clears
 *  a route, so only present crosspoints are emitted). */
export interface NexusCrosspoint {
  in: number;
  out: number;
  gainDb: number;
}

/** audio.routes — the matrix snapshot plus the measured monitor round-trip
 *  (SPEC §6, on change + 1 Hz). `inputs`/`outputs` size the grid; `matrix` is
 *  the sparse list of live crosspoints. `measuredRttMs` is the loopback-impulse
 *  measurement from `monitor.measure` (SPEC §2) — null until the on-device core
 *  has actually measured it; the HUD NEVER fabricates a latency number. */
export interface NexusRoutes {
  inputs: number;
  outputs: number;
  matrix: NexusCrosspoint[];
  measuredRttMs: number | null;
}

/** audio.gain — an input/output trim change (SPEC §6, on change). `stage` is
 *  the gain-staging point (e.g. "interface" | "input_trim" | "output_trim");
 *  kept as a plain string so a novel stage still surfaces. */
export interface NexusGain {
  channel: number;
  gainDb: number;
  stage: string;
}

/** audio.clipping — a true-peak clip event (SPEC §3/§6, on event). `truePeakDbfs`
 *  is the 4x-oversampled true-peak that crossed the -1 dBFS ceiling; drives the
 *  panel clip flash. */
export interface NexusClipping {
  channel: number;
  truePeakDbfs: number;
}

/** audio.spectrum — the 96-band log spectrum (SPEC §6, 30 Hz). `bands` is
 *  exactly NEXUS_SPECTRUM_BANDS dBFS values, low->high frequency. */
export interface NexusSpectrum {
  bands: number[];
}

/** Coerce one untrusted channel-level object into a NexusChannelLevel, or null
 *  if it lacks a usable finite peak (the structural anchor of a meter). `rms`
 *  defaults to the peak when absent so a peak-only tap still meters rather than
 *  dropping. Never throws. */
function coerceChannelLevel(o: Record<string, unknown>): NexusChannelLevel | null {
  const peakDbfs = num(o, "peak_dbfs");
  if (peakDbfs === null) return null;
  return { peakDbfs, rmsDbfs: num(o, "rms_dbfs") ?? peakDbfs };
}

/** Parse an audio.levels payload (DEFAULT topic). Returns null only when `ch`
 *  is absent/non-array (no meter frame to render); an empty channel list is a
 *  valid "device open, no inputs" frame. Channels are coerced item-by-item
 *  (non-conforming entries dropped). The three LUFS readouts stay null when
 *  omitted so the panel distinguishes "not reported yet" from a real value —
 *  it never shows a fake loudness. Never throws on junk. */
export function parseNexusLevels(data: Record<string, unknown>): NexusLevels | null {
  const rawCh = data["ch"];
  if (!Array.isArray(rawCh)) return null;
  const ch: NexusChannelLevel[] = rawCh
    .filter(isPlainObject)
    .map(coerceChannelLevel)
    .filter((c): c is NexusChannelLevel => c !== null);
  return {
    ch,
    lufsM: num(data, "lufs_m"),
    lufsS: num(data, "lufs_s"),
    lufsI: num(data, "lufs_i"),
  };
}

/** Coerce one untrusted crosspoint object into a NexusCrosspoint, or null if it
 *  lacks finite in/out indices or a finite gain (a crosspoint with no location
 *  or no level is not a route). Never throws. */
function coerceCrosspoint(o: Record<string, unknown>): NexusCrosspoint | null {
  const inCh = num(o, "in");
  const outCh = num(o, "out");
  const gainDb = num(o, "gain_db");
  if (inCh === null || outCh === null || gainDb === null) return null;
  return { in: inCh, out: outCh, gainDb };
}

/** Parse an audio.routes payload. Returns null unless `inputs` and `outputs`
 *  are finite (the grid dimensions are the structural anchor). `matrix` is
 *  coerced item-by-item (non-conforming crosspoints dropped) and defaults to []
 *  when absent (a cleared matrix). `measured_rtt_ms` stays null when the
 *  on-device core has not measured the loopback yet — the HUD never invents a
 *  latency. Never throws on junk. */
export function parseNexusRoutes(data: Record<string, unknown>): NexusRoutes | null {
  const rawInputs = num(data, "inputs");
  const rawOutputs = num(data, "outputs");
  if (rawInputs === null || rawOutputs === null) return null;
  // Floor to non-negative integers and clamp to the VIEW cap. The grid is drawn
  // as `inputs x outputs` cells via Array.from({length}), so an unbounded (or,
  // past 2^32, a RangeError-throwing) dimension from a malformed/spoofed frame
  // must never reach the panel render — fail-safe at the parse boundary, mirroring
  // the chart/spectrum caps.
  const inputs = Math.min(NEXUS_MATRIX_DIM_CAP, Math.max(0, Math.floor(rawInputs)));
  const outputs = Math.min(NEXUS_MATRIX_DIM_CAP, Math.max(0, Math.floor(rawOutputs)));
  const rawMatrix = data["matrix"];
  const matrix: NexusCrosspoint[] = Array.isArray(rawMatrix)
    ? rawMatrix
        .filter(isPlainObject)
        .map(coerceCrosspoint)
        .filter((c): c is NexusCrosspoint => c !== null)
    : [];
  return { inputs, outputs, matrix, measuredRttMs: num(data, "measured_rtt_ms") };
}

/** Parse an audio.gain payload. Returns null unless `channel` and `gain_db` are
 *  finite; `stage` defaults to "" when absent. Never throws. */
export function parseNexusGain(data: Record<string, unknown>): NexusGain | null {
  const channel = num(data, "channel");
  const gainDb = num(data, "gain_db");
  if (channel === null || gainDb === null) return null;
  return { channel, gainDb, stage: str(data, "stage") ?? "" };
}

/** Parse an audio.clipping payload. Returns null unless `channel` and
 *  `true_peak_dbfs` are finite (a clip event without a location or a level is
 *  not actionable). Never throws. */
export function parseNexusClipping(data: Record<string, unknown>): NexusClipping | null {
  const channel = num(data, "channel");
  const truePeakDbfs = num(data, "true_peak_dbfs");
  if (channel === null || truePeakDbfs === null) return null;
  return { channel, truePeakDbfs };
}

/** Parse an audio.spectrum payload. Returns null unless `bands` is an array of
 *  EXACTLY NEXUS_SPECTRUM_BANDS finite numbers — a partial/over-long FFT frame
 *  is rejected wholesale (a 96-band strip with holes would misread the
 *  spectrum). Never throws on junk. */
export function parseNexusSpectrum(data: Record<string, unknown>): NexusSpectrum | null {
  const raw = data["bands"];
  if (!Array.isArray(raw) || raw.length !== NEXUS_SPECTRUM_BANDS) return null;
  const bands: number[] = [];
  for (const v of raw) {
    if (typeof v !== "number" || !Number.isFinite(v)) return null;
    bands.push(v);
  }
  return { bands };
}

/* ------------------------------------------------------------------------ *
 * Mark-Forge micro-app payloads (apps/mark-forge — runtime="binary", a       *
 * deterministic CPU/f64 rigid-body physics engine; gpu=false, net_hosts=[]    *
 * offline). The ENGINE is fully verifiable headlessly (cargo build + test);   *
 * what is DEVICE-GATED is the live R3F render of the sandbox at 60fps — the    *
 * HUD's headless preview SUSPENDS the R3F render loop, so on-screen motion is  *
 * verified only on the real Tauri app / M4 Mini. This panel renders           *
 * TELEMETRY-DRIVEN body transforms (pos + orientation per frame), NOT a        *
 * claimed-measured framerate: the numbers below are the simulation's, the      *
 * motion you see is the engine's, and the frame RATE is never asserted here.   *
 *                                                                              *
 * Three relay topics, matching apps/mark-forge/manifest.toml                   *
 * `telemetry_topics` (daemon/src/apps.rs::resolve_topic, default =             *
 * physics.bodies — the first declared). Transcribed VERBATIM from the app's    *
 * wire structs in apps/mark-forge/src/ipc.rs (the Rust serde shapes are        *
 * truth): BodyTransform / BodiesFrame / StepReport / SceneBody /               *
 * SceneTopology, with ShapeTag tagged on `kind`. `pos` is [x,y,z], `quat` is   *
 * [x,y,z,w] (xyzw, w scalar) — fed straight into THREE.Vector3 /               *
 * THREE.Quaternion. Parsed DEFENSIVELY — a malformed/partial payload yields    *
 * null (or drops the offending sub-item), never a throw.                       *
 * ------------------------------------------------------------------------ */

/** Manifest name of the mark-forge micro-app + its three declared topics
 *  (apps/mark-forge/manifest.toml `telemetry_topics`; the Rust-side mirror is
 *  mark_forge::TOPIC_BODIES/TOPIC_STEP/TOPIC_SCENE). physics.bodies is the
 *  DEFAULT relay topic (first declared). */
export const MARK_FORGE_APP = "mark-forge";
export const PHYSICS_TOPIC_BODIES = "physics.bodies";
export const PHYSICS_TOPIC_STEP = "physics.step";
export const PHYSICS_TOPIC_SCENE = "physics.scene";

/** The shape kinds the engine emits (ShapeTag, tag="kind", snake_case on the
 *  wire). Kept as a plain string on the parsed shapes so an unknown future kind
 *  still surfaces rather than being dropped — the panel renders the known three
 *  and skips an unrecognized kind. */
export type PhysicsShapeKind = "sphere" | "cuboid" | "plane";

/** A parsed ShapeTag (apps/mark-forge/src/ipc.rs `ShapeTag`). The wire form is
 *  externally tagged on `kind`:
 *    {"kind":"sphere","radius":f64}
 *    {"kind":"cuboid","half_extents":[f64,f64,f64]}
 *    {"kind":"plane","normal":[f64,f64,f64],"offset":f64}
 *  Modeled as a discriminated union so the panel can pick the mesh + size it. */
export type PhysicsShape =
  | { kind: "sphere"; radius: number }
  | { kind: "cuboid"; halfExtents: [number, number, number] }
  | { kind: "plane"; normal: [number, number, number]; offset: number };

/** One body's render transform (physics.bodies `bodies[]` / ipc.rs
 *  `BodyTransform`). `pos` is [x,y,z]; `quat` is [x,y,z,w] (xyzw, w scalar) —
 *  both feed straight into THREE. `sleeping` dims a settled body. */
export interface PhysicsBody {
  id: number; // BodyId(u32) — serializes transparent as a bare number
  shape: PhysicsShape;
  pos: [number, number, number];
  quat: [number, number, number, number];
  sleeping: boolean;
}

/** physics.bodies — the per-frame render feed (DEFAULT topic; ipc.rs
 *  `BodiesFrame`). Emitted after world.step and on state.get. `frame` is the
 *  cumulative frame counter; `simTime` the simulated seconds; `bodies` every
 *  body's transform in stable id order. */
export interface PhysicsBodiesFrame {
  frame: number;
  simTime: number;
  bodies: PhysicsBody[];
}

/** physics.step — solver/step stats (ipc.rs `StepReport`, emitted with each
 *  world.step). All counters; `lastPenetration` is the worst residual overlap
 *  (metres) after position correction — a solver-quality readout. */
export interface PhysicsStepReport {
  frames: number;
  substeps: number;
  bodies: number;
  contacts: number;
  solverIterations: number;
  lastPenetration: number;
  /** True when the per-substep candidate-pair budget bit (MAX_PAIRS_PER_SUBSTEP) —
   *  a degenerate/over-dense scene was deterministically bounded, signalled rather
   *  than silently mis-simulated. Defaults false (absent on older payloads). */
  pairsCapHit: boolean;
  /** True when the per-substep contact-solve cap bit (MAX_CONTACTS_PER_SUBSTEP). */
  contactCapHit: boolean;
}

/** One body's static topology entry (physics.scene `bodies[]` / ipc.rs
 *  `SceneBody`). `isStatic` is the wire key "static" (Rust `r#static`) — a
 *  static body (e.g. the ground plane / an anchor) the HUD may render
 *  differently and never expects to move. */
export interface PhysicsSceneBody {
  id: number;
  shape: PhysicsShape;
  isStatic: boolean;
}

/** physics.scene — scene topology on change (ipc.rs `SceneTopology`; emitted on
 *  spawn/reset/set.gravity/set.params/state.get and once on connect). Carries
 *  the sim params + every body's static topology so the panel can (re)build its
 *  meshes; the per-frame motion then arrives on physics.bodies. */
export interface PhysicsSceneTopology {
  gravity: [number, number, number];
  dt: number;
  substeps: number;
  bodies: PhysicsSceneBody[];
}

/** Coerce a wire value into a fixed-length tuple of finite numbers, or null if
 *  it is not an array of exactly `len` finite numbers. Used for pos[3]/quat[4]/
 *  half_extents[3]/normal[3]/gravity[3] — a malformed vector drops the body
 *  rather than feeding NaN into a THREE transform. */
function numTuple(v: unknown, len: 3): [number, number, number] | null;
function numTuple(v: unknown, len: 4): [number, number, number, number] | null;
function numTuple(v: unknown, len: number): number[] | null {
  if (!Array.isArray(v) || v.length !== len) return null;
  const out: number[] = [];
  for (const x of v) {
    if (typeof x !== "number" || !Number.isFinite(x)) return null;
    out.push(x);
  }
  return out;
}

/** Coerce one untrusted ShapeTag object into a PhysicsShape, or null if its
 *  `kind` is unknown or its size field is missing/malformed. Never throws — an
 *  unrecognized kind (or a sphere with no radius, a cuboid with a bad
 *  half_extents, …) drops the shape so the body is skipped rather than rendered
 *  at a degenerate size. */
function coercePhysicsShape(v: unknown): PhysicsShape | null {
  if (!isPlainObject(v)) return null;
  const kind = str(v, "kind");
  if (kind === "sphere") {
    const radius = num(v, "radius");
    return radius === null ? null : { kind: "sphere", radius };
  }
  if (kind === "cuboid") {
    const halfExtents = numTuple(v["half_extents"], 3);
    return halfExtents === null ? null : { kind: "cuboid", halfExtents };
  }
  if (kind === "plane") {
    const normal = numTuple(v["normal"], 3);
    const offset = num(v, "offset");
    return normal === null || offset === null ? null : { kind: "plane", normal, offset };
  }
  return null;
}

/** Coerce one untrusted body-transform object into a PhysicsBody, or null if it
 *  lacks a usable id, a renderable shape, or a well-formed pos/quat. `quat`
 *  defaults to the identity [0,0,0,1] when absent (a body with a position but no
 *  reported orientation still renders upright) but is dropped if PRESENT and
 *  malformed. `sleeping` defaults to false. Never throws. */
function coercePhysicsBody(o: Record<string, unknown>): PhysicsBody | null {
  const id = num(o, "id");
  if (id === null) return null;
  const shape = coercePhysicsShape(o["shape"]);
  if (shape === null) return null;
  const pos = numTuple(o["pos"], 3);
  if (pos === null) return null;
  let quat: [number, number, number, number];
  if ("quat" in o && o["quat"] !== undefined) {
    const q = numTuple(o["quat"], 4);
    if (q === null) return null;
    quat = q;
  } else {
    quat = [0, 0, 0, 1];
  }
  return { id, shape, pos, quat, sleeping: bool(o, "sleeping") ?? false };
}

/** Parse a physics.bodies payload (DEFAULT topic; BodiesFrame). Returns null
 *  unless `frame` is a finite number (the structural anchor). `bodies` is
 *  coerced item-by-item — a malformed body (bad shape/pos/quat) is dropped, the
 *  rest still render. The frame is telemetry-driven transforms, NOT a measured
 *  framerate. Never throws on junk. */
export function parsePhysicsBodies(
  data: Record<string, unknown>,
): PhysicsBodiesFrame | null {
  const frame = num(data, "frame");
  if (frame === null) return null;
  const rawBodies = data["bodies"];
  const bodies: PhysicsBody[] = Array.isArray(rawBodies)
    ? rawBodies
        .filter(isPlainObject)
        .map(coercePhysicsBody)
        .filter((b): b is PhysicsBody => b !== null)
    : [];
  return { frame, simTime: num(data, "sim_time") ?? 0, bodies };
}

/** Parse a physics.step payload (StepReport). Returns null unless the six core
 *  counters/stats are finite (a partial stats line is not rendered — mirrors
 *  parseCanvasRenderMs / parseNexusPerf). The two cap-hit flags are optional and
 *  default false (absent on older payloads). Never throws. */
export function parsePhysicsStep(
  data: Record<string, unknown>,
): PhysicsStepReport | null {
  const frames = num(data, "frames");
  const substeps = num(data, "substeps");
  const bodies = num(data, "bodies");
  const contacts = num(data, "contacts");
  const solverIterations = num(data, "solver_iterations");
  const lastPenetration = num(data, "last_penetration");
  if (
    frames === null ||
    substeps === null ||
    bodies === null ||
    contacts === null ||
    solverIterations === null ||
    lastPenetration === null
  ) {
    return null;
  }
  return {
    frames,
    substeps,
    bodies,
    contacts,
    solverIterations,
    lastPenetration,
    pairsCapHit: bool(data, "pairs_cap_hit") ?? false,
    contactCapHit: bool(data, "contact_cap_hit") ?? false,
  };
}

/** Coerce one untrusted scene-body object into a PhysicsSceneBody, or null if it
 *  lacks a usable id or a renderable shape. The wire key is "static" (Rust
 *  `r#static`); it defaults to false when absent. Never throws. */
function coercePhysicsSceneBody(o: Record<string, unknown>): PhysicsSceneBody | null {
  const id = num(o, "id");
  if (id === null) return null;
  const shape = coercePhysicsShape(o["shape"]);
  if (shape === null) return null;
  return { id, shape, isStatic: bool(o, "static") ?? false };
}

/** Parse a physics.scene payload (SceneTopology). Returns null unless `gravity`
 *  is a well-formed [x,y,z] AND `dt` is finite (the sim params are the
 *  structural anchor). `bodies` is coerced item-by-item (malformed topology
 *  entries dropped) and defaults to [] when absent (an empty scene — the initial
 *  drop on connect). Never throws on junk. */
export function parsePhysicsScene(
  data: Record<string, unknown>,
): PhysicsSceneTopology | null {
  const gravity = numTuple(data["gravity"], 3);
  const dt = num(data, "dt");
  if (gravity === null || dt === null) return null;
  const rawBodies = data["bodies"];
  const bodies: PhysicsSceneBody[] = Array.isArray(rawBodies)
    ? rawBodies
        .filter(isPlainObject)
        .map(coercePhysicsSceneBody)
        .filter((b): b is PhysicsSceneBody => b !== null)
    : [];
  return { gravity, dt, substeps: num(data, "substeps") ?? 0, bodies };
}

/* ------------------------------------------------------------------------ *
 * EPISODIC MEMORY + USER MODEL telemetry (Core-A / Core-B). The daemon's      *
 * episodic store (daemon/src/episodic.rs over memory.rs) and user model        *
 * (daemon/src/user_model.rs, consolidated by reflect.rs) emit ACTIVITY-level   *
 * telemetry only — NEVER the episode bodies or the profile entries themselves. *
 * This is the honest privacy line: episodes are REDACTED + LOCAL + AGENT-      *
 * SCOPED in SQLite and surfaced to the user by VOICE ("what do you know about  *
 * me" -> the user_model_query tool; "recall when we…" -> episodic_recall) —    *
 * they are deliberately NOT broadcast over the HUD's read-only ws stream.      *
 * What the wire carries (transcribed from the daemon emit call sites):         *
 *   - system / episodic.recorded {recorded:bool, agent:str} — per COMPLETED    *
 *     turn (main.rs). `recorded` is whether THIS turn became a durable episode  *
 *     (false = gated out: transient screen-read, empty/abandoned turn, voice-id *
 *     UNVERIFIED, or [episodic] disabled). `agent` is the handling agent, the   *
 *     episode's recall scope. NO utterance, NO content — only the bit + agent.  *
 *   - system / user_model.consolidated {entries_written:number} — reflect.rs,  *
 *     after the deterministic consolidation pass folds recent episodes+facts    *
 *     into the bounded compounding profile. The COUNT of entries written, not   *
 *     the entries. NEVER fabricated: only signals that cleared the observation  *
 *     threshold are written.                                                    *
 *   - system / user_model.consolidation_failed {error:string} — the pass could  *
 *     not run this cycle (busy/locked DB). Honest "stale profile" affordance.   *
 *   - system / memory.retention {events_deleted, transcripts_deleted,           *
 *     episodes_deleted} — the bounded evict-oldest retention pass (main.rs).     *
 *     `episodes_deleted` is the PROOF the episodic store is bounded, not        *
 *     "remembers everything".                                                   *
 * Parsed DEFENSIVELY — a malformed/partial payload yields null, never a throw.  *
 * ------------------------------------------------------------------------ */

/** A parsed episodic.recorded event — ONE completed turn's episode-store
 *  outcome. `recorded` is whether the turn became a durable, redacted episode
 *  (false = honestly gated out: transient screen-read, empty/abandoned turn,
 *  voice-id UNVERIFIED, or the store disabled). `agent` is the namespace the
 *  episode (if any) is scoped to. NO content — the utterance/summary stay LOCAL
 *  in the daemon and are surfaced only by voice (episodic_recall). */
export interface EpisodicRecorded {
  recorded: boolean;
  agent: string;
}

/** Parse an episodic.recorded payload. Returns null unless `recorded` is a real
 *  boolean (the structural anchor — without it there is no outcome to surface);
 *  `agent` defaults to "" (the shared scope) when absent. Never throws. */
export function parseEpisodicRecorded(
  data: Record<string, unknown>,
): EpisodicRecorded | null {
  const recorded = bool(data, "recorded");
  if (recorded === null) return null;
  return { recorded, agent: str(data, "agent") ?? "" };
}

/** A parsed user_model.consolidated event — the deterministic consolidation
 *  pass folded recent episodes + facts into the bounded compounding profile.
 *  `entriesWritten` is the COUNT of profile entries written this round (NOT the
 *  entries — those are read by voice via user_model_query). NEVER fabricated. */
export interface UserModelConsolidated {
  entriesWritten: number;
}

/** Parse a user_model.consolidated payload. Returns null unless
 *  `entries_written` is a finite number. Never throws. */
export function parseUserModelConsolidated(
  data: Record<string, unknown>,
): UserModelConsolidated | null {
  const entriesWritten = num(data, "entries_written");
  if (entriesWritten === null) return null;
  return { entriesWritten };
}

/** A parsed memory.retention event — the bounded evict-oldest retention pass.
 *  `episodesDeleted` is the load-bearing field for the memory panel: it is the
 *  PROOF the episodic store is bounded (evicts oldest at the cap), the opposite
 *  of "remembers everything". The other two counters cover events/transcripts. */
export interface MemoryRetention {
  eventsDeleted: number;
  transcriptsDeleted: number;
  episodesDeleted: number;
}

/** Parse a memory.retention payload. Returns null unless at least one counter is
 *  present (an all-absent payload is not a real pass). Missing counters default
 *  to 0. Never throws. */
export function parseMemoryRetention(
  data: Record<string, unknown>,
): MemoryRetention | null {
  const eventsDeleted = num(data, "events_deleted");
  const transcriptsDeleted = num(data, "transcripts_deleted");
  const episodesDeleted = num(data, "episodes_deleted");
  if (eventsDeleted === null && transcriptsDeleted === null && episodesDeleted === null) {
    return null;
  }
  return {
    eventsDeleted: eventsDeleted ?? 0,
    transcriptsDeleted: transcriptsDeleted ?? 0,
    episodesDeleted: episodesDeleted ?? 0,
  };
}

/* ------------------------------------------------------------------------ *
 * MCP (Model Context Protocol) — daemon/src/mcp.rs::McpManager::status_snapshot. *
 * The daemon emits ONE `system / mcp.status` event after startup connect: the   *
 * external-tool surface, for the read-only HUD MCP panel. It is SECRET-FREE by  *
 * construction on the daemon side (only `uses_token` as a bool — never a token  *
 * value, Keychain account, or credentialed URL), and the parser below carries   *
 * that contract forward: it surfaces ONLY name/transport/connected/uses_token/  *
 * agents/tools, so even a malformed payload can never smuggle a secret field    *
 * into the panel. SHIPPED-OFF default: enabled=false, servers=[] — an honest    *
 * "MCP is off" snapshot.                                                         *
 * ------------------------------------------------------------------------ */

/** One tool a connected MCP server exposes — name plus its safety class. The
 *  panel badges a consequential tool (it parks behind the confirmation gate)
 *  distinctly from a read-only one. */
export interface McpToolStatus {
  name: string;
  consequential: boolean;
}

/** One configured MCP server's status (mcp.status `servers[]`). `connected` is
 *  whether it handshook at startup; `tools` is empty until/unless connected.
 *  `usesToken` is a BOOL only — the panel NEVER renders a token/secret (there is
 *  no token field on the wire to render). `agents` is the per-server allowlist:
 *  which JARVIS agents may use this server's tools. */
export interface McpServerStatus {
  name: string;
  transport: string; // "stdio" | "http" (tolerant of future kinds)
  connected: boolean;
  usesToken: boolean;
  agents: string[];
  tools: McpToolStatus[];
}

/** The whole MCP surface (mcp.status). `enabled` is the `[mcp].enabled` master
 *  switch (ships false). `servers` is every CONFIGURED server (connected or not)
 *  so the panel can show "configured but not connected" honestly. */
export interface McpStatus {
  enabled: boolean;
  servers: McpServerStatus[];
}

/** Coerce one untrusted tool object into an McpToolStatus, or null if it has no
 *  usable name (a nameless tool is not addressable, so it is not shown). `class`
 *  defaults to consequential (fail-safe) when the bool is absent — the panel
 *  never under-states a tool's risk. Never throws. */
/* -------------------------------------------------------------------------- *
 * capability.atlas — the unified ARMED/INERT capability surface (atlas.rs).   *
 * -------------------------------------------------------------------------- */

/** One capability in the atlas: name, kind, armed verdict, and a secret-free
 *  one-line detail (what it is when armed, or WHY it is inert when not). */
export interface CapabilityEntry {
  name: string;
  /** "skill" | "agent" | "app" | "integration" — tolerant of future kinds. */
  kind: string;
  armed: boolean;
  detail: string;
}

/** The capability.atlas snapshot: the master switch, armed/total counts, and the
 *  per-capability entries. */
export interface CapabilityAtlas {
  enabled: boolean;
  armed: number;
  total: number;
  capabilities: CapabilityEntry[];
}

/** Coerce one untrusted capability object, or null if it has no usable name (an
 *  unnamed entry is not addressable). Every other field defaults safely. Surfaces
 *  ONLY the secret-free fields. Never throws. */
function coerceCapability(o: Record<string, unknown>): CapabilityEntry | null {
  const name = str(o, "name");
  if (name === null || name.length === 0) return null;
  return {
    name,
    kind: str(o, "kind") ?? "unknown",
    armed: bool(o, "armed") ?? false,
    detail: str(o, "detail") ?? "",
  };
}

/** Parse a capability.atlas payload. NEVER returns null — a malformed payload
 *  yields an empty, honest snapshot rather than a stale one. `enabled` defaults
 *  to false; the counts fall back to derived values; malformed entries dropped.
 *  Never carries a secret. Never throws on junk. */
/** Bound the atlas so a hostile capability.atlas frame can't flood state/DOM. */
const CAPABILITY_ATLAS_CAP = 500;

export function parseCapabilityAtlas(data: Record<string, unknown>): CapabilityAtlas {
  const rawCaps = data["capabilities"];
  const capabilities = Array.isArray(rawCaps)
    ? rawCaps
        .filter(isPlainObject)
        .map(coerceCapability)
        .filter((c): c is CapabilityEntry => c !== null)
        .slice(0, CAPABILITY_ATLAS_CAP)
    : [];
  return {
    enabled: bool(data, "enabled") ?? false,
    armed: num(data, "armed") ?? capabilities.filter((c) => c.armed).length,
    total: num(data, "total") ?? capabilities.length,
    capabilities,
  };
}

// ---------------------------------------------------------------------------
// TCC PERMISSION SENTINEL (tcc.snapshot / tcc.anomaly) — the ambient READ-ONLY
// macOS app-privacy-grant status + new-grant/escalation alerts (fed by
// daemon/src/tcc.rs). SECRET-FREE: bundle ids + counts only, never a token. The
// parsers never throw; the status never returns null, so an unreadable TCC store
// (needs Full Disk Access) is shown honestly rather than as a stale panel.
// ---------------------------------------------------------------------------

/** tcc.snapshot — the ambient permission status. `available=false` means macOS
 *  blocked the read (grant Full Disk Access); the counts are meaningful only
 *  when `available` is true. */
export interface TccSentinel {
  available: boolean;
  grants: number;
  highRiskAllowed: number;
}

/** Coerce a tcc.snapshot payload. NEVER returns null — an unavailable/malformed
 *  read yields an honest `available=false` snapshot, never a stale one. */
export function parseTccSnapshot(data: Record<string, unknown>): TccSentinel {
  const available = bool(data, "available") ?? false;
  return {
    available,
    grants: available ? num(data, "grants") ?? 0 : 0,
    highRiskAllowed: available ? num(data, "high_risk_allowed") ?? 0 : 0,
  };
}

/** Max anomaly lines retained/rendered (bounds an unbounded alert history). */
export const TCC_ANOMALY_CAP = 20;

/** Coerce a tcc.anomaly payload into its human-readable alert lines (new grant /
 *  denied→allowed escalation). Drops non-strings; caps the list. Never throws. */
export function parseTccAnomalies(data: Record<string, unknown>): string[] {
  const raw = data["items"];
  if (!Array.isArray(raw)) return [];
  return raw
    .filter((x): x is string => typeof x === "string" && x.length > 0)
    .slice(0, TCC_ANOMALY_CAP);
}

// ---------------------------------------------------------------------------
// MICRO-APP INTROSPECTION (introspect.snapshot / introspect.profile_drift /
// introspect.anomaly / introspect.module_violation) — the ambient READ-ONLY
// sentinel over jarvisd's OWN sandboxed children (daemon/src/introspect.rs):
// SBPL profile-drift, RSS/CPU anomalies, and cooperative dyld module attestation.
// SECRET-FREE: app names + counts + module paths only, never a token or file
// contents. Parsers never throw; the status never returns null.
// ---------------------------------------------------------------------------

/** introspect.snapshot — the per-tick tally of the sandboxed-child sentinel. */
export interface IntrospectStatus {
  apps: number;
  drift: number;
  anomalies: number;
}

/** Coerce an introspect.snapshot payload. NEVER null — a malformed payload
 *  yields an honest all-zero snapshot, never a stale one. */
export function parseIntrospectSnapshot(data: Record<string, unknown>): IntrospectStatus {
  return {
    apps: num(data, "apps") ?? 0,
    drift: num(data, "drift") ?? 0,
    anomalies: num(data, "anomalies") ?? 0,
  };
}

/** Max introspect finding lines retained/rendered. */
export const INTROSPECT_ALERT_CAP = 20;

/** Format an introspect.profile_drift payload into a finding line, or null if it
 *  has no app name (the structural anchor). */
export function introspectDriftLine(data: Record<string, unknown>): string | null {
  const app = str(data, "app");
  if (app === null || app.length === 0) return null;
  return bool(data, "missing")
    ? `PROFILE MISSING: ${app} — its seatbelt profile file is gone`
    : `PROFILE DRIFT: ${app} — on-disk seatbelt profile changed since launch`;
}

/** Format an introspect.anomaly payload into a finding line, or null if unusable. */
export function introspectAnomalyLine(data: Record<string, unknown>): string | null {
  const app = str(data, "app");
  const kind = str(data, "kind");
  if (app === null || app.length === 0 || kind === null || kind.length === 0) return null;
  const detail = str(data, "detail") ?? "";
  return `ANOMALY [${kind}]: ${app}${detail ? ` — ${detail}` : ""}`;
}

/** Format an introspect.module_violation payload into a finding line, or null. */
export function introspectModuleViolationLine(data: Record<string, unknown>): string | null {
  const app = str(data, "app");
  const path = str(data, "path");
  if (app === null || app.length === 0 || path === null || path.length === 0) return null;
  return `MODULE: ${app} loaded unexpected ${path}`;
}

/** Format an introspect.security_event payload (kernel security event about a
 *  tracked app — W^X violation / task-port acquisition / signal) into a finding
 *  line, or null. `high` events are tagged SECURITY so the panel highlights them. */
export function introspectSecurityLine(data: Record<string, unknown>): string | null {
  const app = str(data, "app");
  const kind = str(data, "kind");
  if (app === null || app.length === 0 || kind === null || kind.length === 0) return null;
  const detail = str(data, "detail") ?? "";
  const tag = bool(data, "high") ? "SECURITY" : "notice";
  return `${tag} [${kind}]: ${app}${detail ? ` — ${detail}` : ""}`;
}

/** Accumulate a finding newest-first, deduped, capped. A persistent finding
 *  re-fires every tick but dedupe collapses it to one line. */
export function mergeIntrospectAlert(line: string, prev: string[]): string[] {
  return [line, ...prev]
    .filter((x, i, a) => a.indexOf(x) === i)
    .slice(0, INTROSPECT_ALERT_CAP);
}

/** One app's DECLARED capabilities (introspect.capabilities) — the "what can this
 *  app do" audit from its manifest. `caps` is a compact secret-free summary. */
export interface IntrospectCapability {
  name: string;
  caps: string;
}

/** Coerce an introspect.capabilities payload (`{apps:[{name,caps}]}`) into a
 *  sorted, de-duplicated list; drops entries with no usable name. Never throws. */
export function parseIntrospectCapabilities(
  data: Record<string, unknown>,
): IntrospectCapability[] {
  const raw = data["apps"];
  if (!Array.isArray(raw)) return [];
  const out: IntrospectCapability[] = [];
  const seen = new Set<string>();
  for (const item of raw) {
    if (!isPlainObject(item)) continue;
    const name = str(item, "name");
    if (name === null || name.length === 0 || seen.has(name)) continue;
    seen.add(name);
    out.push({ name, caps: str(item, "caps") ?? "" });
  }
  return out.sort((a, b) => a.name.localeCompare(b.name));
}

// ---------------------------------------------------------------------------
// CAPABILITY ATTRIBUTION HEALTH (attribution.health) — the PROPOSE-ONLY ambient
// signal of which of JARVIS's own agents/skills are reliable vs failing, from
// the trace corpus (daemon/src/attribution.rs). Counts + failing-capability
// flags only — no secret, no raw utterance. Parsers never throw.
// ---------------------------------------------------------------------------

/** One well-sampled capability the sentinel flagged as FAILING. */
export interface AttributionFlag {
  /** "agent" | "tool" (or a future kind — surfaced verbatim). */
  kind: string;
  name: string;
  turns: number;
  /** Success rate as a whole-number percent. */
  rate: number;
}

/** attribution.health — the ambient capability-health snapshot. */
export interface AttributionHealth {
  turns: number;
  reliable: number;
  /** Well-sampled capabilities in the mediocre [50%,80%) band — reported so the
   *  snapshot accounts for every judged capability, not just reliable + failing. */
  mixed: number;
  failing: number;
  flags: AttributionFlag[];
  /** Eval-verified skills that are also live-proven — ready-to-promote. */
  promote: AttributionFlag[];
}

/** Max failing-capability flags retained/rendered. */
export const ATTRIBUTION_FLAG_CAP = 12;

/** Coerce one failing-capability flag, or null if it has no usable name. */
function coerceAttributionFlag(o: Record<string, unknown>): AttributionFlag | null {
  const name = str(o, "name");
  if (name === null || name.length === 0) return null;
  return {
    kind: str(o, "kind") ?? "capability",
    name,
    // turns + rate are whole-number counts/percents — truncate a float or
    // negative payload rather than render "6.7 turns · 45.7% success".
    turns: nonNegIntOr0(o, "turns"),
    rate: nonNegIntOr0(o, "rate"),
  };
}

/** Coerce an attribution.health payload. NEVER returns null — a malformed
 *  payload yields an honest all-zero snapshot, never a stale one. */
function coerceFlagList(raw: unknown): AttributionFlag[] {
  return Array.isArray(raw)
    ? raw
        .filter(isPlainObject)
        .map(coerceAttributionFlag)
        .filter((f): f is AttributionFlag => f !== null)
        .slice(0, ATTRIBUTION_FLAG_CAP)
    : [];
}

export function parseAttributionHealth(data: Record<string, unknown>): AttributionHealth {
  return {
    turns: num(data, "turns") ?? 0,
    reliable: num(data, "reliable") ?? 0,
    mixed: num(data, "mixed") ?? 0,
    failing: num(data, "failing") ?? 0,
    flags: coerceFlagList(data["flags"]),
    promote: coerceFlagList(data["promote"]),
  };
}

function coerceMcpTool(o: Record<string, unknown>): McpToolStatus | null {
  const name = str(o, "name");
  if (name === null || name.length === 0) return null;
  return { name, consequential: bool(o, "consequential") ?? true };
}

/** Coerce one untrusted server object into an McpServerStatus, or null if it has
 *  no usable name (the structural anchor — an unnamed server is not addressable).
 *  Every other field defaults safely: transport "stdio", disconnected, no token,
 *  empty allowlist, no tools. Tools/agents are coerced item-by-item (junk entries
 *  dropped). DELIBERATELY surfaces ONLY the secret-free fields — any extra field
 *  on the wire (there is none today) is ignored, so the panel can never render a
 *  secret. Never throws. */
// Bound the MCP snapshot so a hostile mcp.status frame can't flood state/DOM.
const MCP_SERVERS_CAP = 64;
const MCP_AGENTS_CAP = 64;
const MCP_TOOLS_CAP = 256;

function coerceMcpServer(o: Record<string, unknown>): McpServerStatus | null {
  const name = str(o, "name");
  if (name === null || name.length === 0) return null;
  const rawAgents = o["agents"];
  const agents = Array.isArray(rawAgents)
    ? rawAgents.filter((x): x is string => typeof x === "string").slice(0, MCP_AGENTS_CAP)
    : [];
  const rawTools = o["tools"];
  const tools = Array.isArray(rawTools)
    ? rawTools
        .filter(isPlainObject)
        .map(coerceMcpTool)
        .filter((t): t is McpToolStatus => t !== null)
        .slice(0, MCP_TOOLS_CAP)
    : [];
  return {
    name,
    transport: str(o, "transport") ?? "stdio",
    connected: bool(o, "connected") ?? false,
    usesToken: bool(o, "uses_token") ?? false,
    agents,
    tools,
  };
}

/** Parse an mcp.status payload. `enabled` defaults to false (the shipped-OFF
 *  posture) when absent/non-bool. `servers` defaults to [] and is coerced
 *  item-by-item (malformed entries dropped). NEVER returns null — an MCP status
 *  frame always yields a (possibly empty) snapshot so the panel can render the
 *  honest "off / no servers" state rather than a stale one. NEVER carries a
 *  secret. Never throws on junk. */
export function parseMcpStatus(data: Record<string, unknown>): McpStatus {
  const rawServers = data["servers"];
  const servers = Array.isArray(rawServers)
    ? rawServers
        .filter(isPlainObject)
        .map(coerceMcpServer)
        .filter((s): s is McpServerStatus => s !== null)
        .slice(0, MCP_SERVERS_CAP)
    : [];
  return { enabled: bool(data, "enabled") ?? false, servers };
}

/* ------------------------------------------------------------------------ *
 * EXTENSIBILITY (#35 webhook triggers + #36 plugin SDK) — the two INBOUND-/    *
 * MODULE-surface event streams the daemon emits for the read-only HUD          *
 * extensibility panel. BOTH are secret-free BY CONSTRUCTION on the daemon side *
 * and the parsers below carry that contract forward — no body, no secret, no   *
 * signature, no capability token ever crosses the wire or reaches state.       *
 *                                                                              *
 * #35 webhook.received (daemon/src/webhooks.rs::emit_decision):                 *
 *   {outcome, event, intent} where outcome ∈ routed|parked|unauthorized|       *
 *   unmapped|bad_request. event+intent+outcome ONLY — NEVER the body/secret/    *
 *   signature. The panel ACCUMULATES these into a running events-received count *
 *   + the last-event {outcome,event,intent} so it can show listener liveness    *
 *   ("an event arrived => the loopback listener is bound") + the last intent    *
 *   (never the payload). A webhook NEVER auto-runs a consequential action — a   *
 *   consequential mapping reads as `parked` (it parks for the user's confirm).  *
 *                                                                              *
 * #36 plugin.handshake (daemon/src/main.rs register-on-launch wiring):          *
 *   {name, status, detail} where status ∈ admitted|invalid_manifest|           *
 *   unauthorized and detail is an intent count ("N intents") or the precise     *
 *   manifest error — NEVER the capability token. The panel ACCUMULATES the      *
 *   latest handshake per plugin name so it can list the installed, validated,   *
 *   SBPL-sandboxed capability modules (admitted) and surface a rejected one     *
 *   honestly (invalid_manifest / unauthorized) — never a fabricated plugin.     *
 *                                                                              *
 * Both subsystems ship OFF/opt-in; with the flags off NO event arrives, so the *
 * panel renders an honest "no event received yet / nothing installed" state.   *
 * ------------------------------------------------------------------------ */

/** The outcome of a single received webhook (webhook.received `outcome`). A
 *  closed union mirroring webhooks.rs::WebhookDecision — an unknown outcome is
 *  dropped by the parser (the panel never renders an unrecognized badge). */
export type WebhookOutcome =
  | "routed"
  | "parked"
  | "unauthorized"
  | "unmapped"
  | "bad_request";

const WEBHOOK_OUTCOMES: readonly WebhookOutcome[] = [
  "routed",
  "parked",
  "unauthorized",
  "unmapped",
  "bad_request",
];

/** One received webhook's secret-free decision (webhook.received). `event` and
 *  `intent` are the mapping LABELS only — never the body, secret, or signature
 *  (the daemon strips those before emitting; a reject outcome carries empty
 *  strings). */
export interface WebhookEvent {
  outcome: WebhookOutcome;
  event: string; // the inbound event label ("" on a reject that has none)
  intent: string; // the mapped intent ("" when unmapped/unauthorized/bad)
}

/** The accumulated WEBHOOKS surface for the panel. `received` is the running
 *  count of decisions seen this session; `last` is the most recent decision (or
 *  null before any). An event having arrived at all means the loopback listener
 *  is bound — until then the panel shows the honest OFF/idle state. SECRET-FREE:
 *  there is no body/secret field anywhere in this shape. */
export interface WebhookSurface {
  received: number;
  last: WebhookEvent | null;
}

/** Parse one webhook.received payload into a secret-free WebhookEvent, or null
 *  if the outcome is missing/unrecognized (an un-actionable frame is dropped,
 *  not guessed). `event`/`intent` default to "" (a reject path carries none).
 *  DELIBERATELY surfaces ONLY outcome/event/intent — any extra field a malformed
 *  payload might carry (a body/secret) is ignored, so the panel can never render
 *  one. Never throws. */
export function parseWebhookEvent(
  data: Record<string, unknown>,
): WebhookEvent | null {
  const outcome = str(data, "outcome");
  if (outcome === null || !WEBHOOK_OUTCOMES.includes(outcome as WebhookOutcome)) {
    return null;
  }
  return {
    outcome: outcome as WebhookOutcome,
    event: str(data, "event") ?? "",
    intent: str(data, "intent") ?? "",
  };
}

/** Fold a freshly-parsed WebhookEvent into the accumulated surface: bump the
 *  count and replace `last`. Pure — the reducer owns the state cell. */
export function applyWebhookEvent(
  prev: WebhookSurface,
  ev: WebhookEvent,
): WebhookSurface {
  return { received: prev.received + 1, last: ev };
}

/** The initial WEBHOOKS surface — no event seen, listener idle/off. */
export function webhookSurfaceInitial(): WebhookSurface {
  return { received: 0, last: null };
}

/** The status of a plugin's register-on-launch handshake (plugin.handshake
 *  `status`). A closed union mirroring plugin_sdk.rs::HandshakeOutcome — an
 *  unknown status is dropped by the parser. */
export type PluginHandshakeStatus =
  | "admitted"
  | "invalid_manifest"
  | "unauthorized";

const PLUGIN_STATUSES: readonly PluginHandshakeStatus[] = [
  "admitted",
  "invalid_manifest",
  "unauthorized",
];

/** One installed capability module's latest handshake (plugin.handshake).
 *  `name` is the registry name; `status` the validation/auth outcome; `detail`
 *  is an intent count ("N intents") or the precise manifest error — NEVER the
 *  capability token (the daemon never puts it on the wire). `intents` is the
 *  count parsed out of `detail` when admitted, for the panel's count badge (null
 *  when not admitted or unparseable). */
export interface PluginRecord {
  name: string;
  status: PluginHandshakeStatus;
  detail: string;
  intents: number | null;
}

/** The accumulated PLUGINS surface — the latest handshake per module name. Null
 *  until the first handshake arrives (the SDK ships OFF, so nothing registers
 *  until enabled). SECRET-FREE — no token field anywhere. */
export interface PluginSurface {
  modules: PluginRecord[];
}

/** Pull the leading integer out of an admitted handshake's `detail` ("N
 *  intents") for the count badge; null if absent/unparseable. */
function intentsFromDetail(detail: string): number | null {
  const m = detail.match(/^\s*(\d+)\b/);
  if (m === null) return null;
  const n = Number.parseInt(m[1], 10);
  return Number.isFinite(n) ? n : null;
}

/** Parse one plugin.handshake payload into a secret-free PluginRecord, or null
 *  if the name is missing (an unnamed module is not addressable) or the status
 *  is unrecognized. `detail` defaults to "". DELIBERATELY surfaces ONLY
 *  name/status/detail (+ the derived intent count) — any extra field (a token)
 *  a malformed payload might carry is ignored. Never throws. */
export function parsePluginHandshake(
  data: Record<string, unknown>,
): PluginRecord | null {
  const name = str(data, "name");
  if (name === null || name.length === 0) return null;
  const status = str(data, "status");
  if (
    status === null ||
    !PLUGIN_STATUSES.includes(status as PluginHandshakeStatus)
  ) {
    return null;
  }
  const detail = str(data, "detail") ?? "";
  return {
    name,
    status: status as PluginHandshakeStatus,
    detail,
    intents: status === "admitted" ? intentsFromDetail(detail) : null,
  };
}

/** Fold a freshly-parsed PluginRecord into the accumulated surface: replace any
 *  prior record for the same name with the latest handshake (a re-launch updates
 *  in place), else append. Stable order (existing names keep position). Pure. */
export function applyPluginHandshake(
  prev: PluginSurface | null,
  rec: PluginRecord,
): PluginSurface {
  const modules = prev ? prev.modules.slice() : [];
  const i = modules.findIndex((m) => m.name === rec.name);
  if (i >= 0) modules[i] = rec;
  else modules.push(rec);
  return { modules };
}

/* ------------------------------------------------------------------------ *
 * AT-REST ENCRYPTION (#11) — daemon/src/main.rs `system / security.status`.       *
 * The daemon emits ONE secret-free `security.status` event after startup connect: *
 * the at-rest-encryption posture, for the HUD ENCRYPTED AT REST / NOT ENCRYPTED   *
 * indicator + the Settings encryption section. It is SECRET-FREE by construction  *
 * on the daemon side — it NEVER carries the master key (the key lives only in the *
 * macOS Keychain); only booleans, the honest scope arrays, and the verbatim       *
 * honesty/key-location/cipher strings cross the wire. The parser below carries    *
 * that contract forward: it surfaces ONLY those fields, so even a malformed       *
 * payload can never smuggle a key-shaped field into the panel.                    *
 *                                                                                 *
 * HONESTY (do not over-state on the way to the HUD): `active` is the GROUND TRUTH *
 * (the key actually RESOLVED this run) — NOT merely `encrypt_memory_config`, so a *
 * config-on-but-key-failed session reads honestly as NOT ENCRYPTED. The four      *
 * sensitive SQLite stores + the voiceid owner blob are encrypted AT REST ON DISK  *
 * with transparent whole-file SQLCipher AES-256; the config TOML, the Keychain    *
 * item itself, and — critically — the in-RAM working set + decrypted pages + the  *
 * key while jarvisd runs are NOT protected. Ships OFF + opt-in. The indicator     *
 * copy must say all of this plainly and never claim "all your data is encrypted". *
 * ------------------------------------------------------------------------ */

/** The at-rest-encryption surface (security.status). SECRET-FREE — there is no
 *  key field on the wire to render. `config` is the `[security].encrypt_memory`
 *  switch (intent); `active` is the GROUND TRUTH (the master key actually
 *  resolved this run), which is what the indicator renders — config-on but
 *  key-failed reads honestly as NOT active. `encryptedStores` / `notEncrypted`
 *  are the honest scope arrays; `honesty` / `keyLocation` / `cipher` are the
 *  verbatim daemon strings the panel surfaces. */
export interface SecurityStatus {
  config: boolean;
  active: boolean;
  encryptedStores: string[];
  notEncrypted: string[];
  honesty: string;
  keyLocation: string;
  cipher: string;
}

/** Parse a security.status payload. NEVER returns null — a security frame always
 *  yields a snapshot so the indicator renders the honest current posture (OFF /
 *  encrypted) rather than a stale one. `config`/`active` default to FALSE (the
 *  shipped-OFF, fail-honest posture) when absent/non-bool — a missing/garbled
 *  `active` must NEVER read as ENCRYPTED (fail toward the honest "not encrypted"
 *  state, never toward a false all-clear). The scope arrays are coerced
 *  string-by-string (junk dropped); the honesty/key/cipher strings default to
 *  "". DELIBERATELY surfaces ONLY these fields — any extra (key-shaped) field on
 *  the wire (there is none today) is IGNORED, so the panel can never render a
 *  secret. Never throws on junk. */
export function parseSecurityStatus(data: Record<string, unknown>): SecurityStatus {
  return {
    config: bool(data, "encrypt_memory_config") ?? false,
    active: bool(data, "active") ?? false,
    encryptedStores: strArr(data, "encrypted_stores") ?? [],
    notEncrypted: strArr(data, "not_encrypted") ?? [],
    honesty: str(data, "honesty") ?? "",
    keyLocation: str(data, "key_location") ?? "",
    cipher: str(data, "cipher") ?? "",
  };
}

/** The at-a-glance tone for the encryption chip/pill: "on" (ENCRYPTED AT REST —
 *  the key resolved + the stores opened encrypted), or "idle" (NOT ENCRYPTED —
 *  the shipped-OFF default, or config-on but the key failed). Deliberately not
 *  an alarm colour: NOT ENCRYPTED is the honest, shipped default, not an error. */
export function securityTone(s: SecurityStatus): "on" | "idle" {
  return s.active ? "on" : "idle";
}

/** The indicator label: ENCRYPTED AT REST when the key actually resolved, else
 *  NOT ENCRYPTED. Driven by `active` (ground truth), NEVER by `config` alone —
 *  config-on but key-failed is honestly NOT ENCRYPTED. */
export function securityLabel(s: SecurityStatus): string {
  return s.active ? "ENCRYPTED AT REST" : "NOT ENCRYPTED";
}

/* ------------------------------------------------------------------------ *
 * PANIC / LOCKDOWN (#12) — daemon/src/lockdown.rs + main.rs + router.rs + the      *
 * command channel. THE emergency stop. While ENGAGED, the daemon forces OFF every  *
 * consequential / outward / autonomy / mic surface — no exception — and it         *
 * PERSISTS across a restart (a disk marker) until an explicit, deliberate,         *
 * USER-ONLY unlock. The HUD's job here is the OBSERVABLE face of that: a LOCKED     *
 * DOWN / NORMAL indicator, a prominent PANIC control, and a deliberate UNLOCK.     *
 *                                                                                  *
 * The daemon feeds the HUD three secret-free signals:                              *
 *   1. STARTUP — `system / lockdown.status` { locked, restored_from_marker },      *
 *      emitted once after telemetry::init (shipped default {false,false}). This    *
 *      drives the indicator AND signals a restart that came up still locked.       *
 *   2. ON TRIGGER (voice) — `system / lockdown.panic` {via:"voice"} /              *
 *      `lockdown.unlock` {via:"voice"} from the router; `command.routed`           *
 *      {cmd:"panic"|"unlock"} when the HUD button fires.                           *
 *   3. The panic/unlock COMMAND REPLY carries `locked`, so the indicator flips     *
 *      immediately on a button press (see tauri/command.ts).                       *
 *                                                                                  *
 * HONESTY (pinned in the verbatim consts below, echoed by the HUD): panic stops    *
 * ALL FUTURE outward actions + autonomy + the mic immediately, and persists — it   *
 * does NOT and CANNOT undo an action already executed (a sent message stays sent). *
 * Unlock is user-only + deliberate and RESTORES your configured settings (lockdown *
 * was an overlay — nothing was changed underneath them). With lockdown OFF (the    *
 * shipped default) behavior is byte-for-byte today.                                *
 * ------------------------------------------------------------------------ */

/** The verbatim spoken confirmation the daemon's `lockdown::panic()` returns
 *  (daemon/src/lockdown.rs `PANIC_CONFIRMATION`). The HUD echoes this EXACT copy
 *  so the spoken line and the on-screen confirmation are one honest message: it
 *  names what the stop does AND, critically, what it does NOT do (it can't un-send
 *  an email). Kept in lockstep with the daemon const. */
export const PANIC_CONFIRMATION =
  "Lockdown engaged. I've stopped all future outward actions, all autonomy, and " +
  "the microphone immediately, and this persists across a restart until you " +
  "unlock. I can't undo anything already done — a sent message stays sent. Say " +
  "'unlock' or use the panic control in Settings to resume.";

/** The verbatim spoken confirmation the daemon's `lockdown::unlock()` returns
 *  (daemon/src/lockdown.rs `UNLOCK_CONFIRMATION`). Echoed by the HUD: unlock is an
 *  OVERLAY lift, so your configured switches return exactly as they were —
 *  nothing was mutated underneath them. */
export const UNLOCK_CONFIRMATION =
  "Lockdown lifted. Your configured settings are restored — nothing was changed " +
  "underneath them.";

/** The emergency-stop posture (lockdown.status). SECRET-FREE — booleans only.
 *  `locked` is the ground truth (the process-global flag, the daemon's
 *  is_locked_down()); `restoredFromMarker` is true only on a startup snapshot
 *  that RE-ENTERED lockdown from the persisted marker (a restart that came up
 *  still locked), which the HUD surfaces so a user knows the stop survived a
 *  reboot. */
export interface LockdownStatus {
  locked: boolean;
  restoredFromMarker: boolean;
}

/** Parse a lockdown.status payload. NEVER returns null — a lockdown frame always
 *  yields a snapshot so the indicator renders the current honest posture rather
 *  than a stale one. `locked` defaults to FALSE (the shipped-OFF default) when
 *  absent/non-bool, and `restoredFromMarker` to FALSE. Surfaces ONLY these two
 *  booleans; any extra field on the wire is ignored. Never throws on junk. */
export function parseLockdownStatus(data: Record<string, unknown>): LockdownStatus {
  return {
    locked: bool(data, "locked") ?? false,
    restoredFromMarker: bool(data, "restored_from_marker") ?? false,
  };
}

/** The at-a-glance tone for the lockdown indicator: "bad" (LOCKED DOWN — the
 *  emergency stop is engaged; this IS an alarm state, unlike the encryption
 *  chip's idle/off) or "ok" (NORMAL — the shipped default, every gate exactly as
 *  configured). */
export function lockdownTone(l: LockdownStatus): "bad" | "ok" {
  return l.locked ? "bad" : "ok";
}

/** The indicator label: LOCKED DOWN when engaged, else NORMAL. */
export function lockdownLabel(l: LockdownStatus): string {
  return l.locked ? "LOCKED DOWN" : "NORMAL";
}

/** The shipped-OFF default snapshot — NORMAL, not restored. Used as the HUD's
 *  honest starting posture before any lockdown.status arrives is `null`, but a
 *  caller that needs a concrete default (e.g. a test) can use this. */
export function lockdownInitial(): LockdownStatus {
  return { locked: false, restoredFromMarker: false };
}

/* ------------------------------------------------------------------------ *
 * VOICE-ID — daemon/src/voiceid.rs + main.rs::handle_voice_id.                   *
 * On-device speaker verification. The daemon emits a SECRET-FREE                 *
 * `system / voiceid.verify` event each turn ({verified, score, enabled,         *
 * enrolled}) plus an enrollment lifecycle (voiceid.enroll_started / _progress / *
 * voiceid.enrolled / voiceid.forgot). NONE of these carry the embedding or any   *
 * audio — only the verdict, a similarity SCORE, and the on/enrolled flags. The   *
 * parsers below carry that contract forward: they surface ONLY those fields, so  *
 * even a malformed payload cannot smuggle an extra field into the indicator.     *
 *                                                                                *
 * HONESTY (do not over-state on the way to the HUD): the shipped embedding is a  *
 * LIGHTWEIGHT acoustic match (filterbank statistics + cosine), NOT a deep        *
 * neural speaker-verification net and NOT a high-assurance biometric. `score`    *
 * is a cosine SIMILARITY to the enrolled profile in [0,1] — it RAISES THE BAR    *
 * (an obviously different voice is rejected) but is spoofable by a recording or  *
 * a good impression. It is NEVER a security guarantee. The hard backstop for     *
 * outward actions remains the OFF-by-default consequential gate + master switch; *
 * voice-id is an ADDED layer. The indicator copy must say this plainly.          *
 * ------------------------------------------------------------------------ */

/** The live voice-id surface the HUD shows. Folds the per-turn verdict
 *  (`voiceid.verify`) together with the enrollment lifecycle so the indicator
 *  can render one of: OFF (`!enabled`), NOT ENROLLED (`enabled && !enrolled`),
 *  ENROLLING (a capture session is in progress), or — once enrolled — this
 *  turn's VERIFIED✓ / UNRECOGNIZED verdict with its similarity score.
 *
 *  `enabled`/`enrolled` mirror the daemon's `[voice_id].enabled` master switch
 *  and "is a profile on file" — they are the authoritative on/off state. The
 *  verdict fields (`verified`/`score`) are meaningful ONLY when enrolled; they
 *  decay to a resting "enrolled, awaiting next utterance" look after a turn.
 *  `score` is a SIMILARITY in [0,1], never a guarantee. */
export interface VoiceIdStatus {
  /** `[voice_id].enabled` — the master switch (ships false). When false the
   *  subsystem enforces nothing; the indicator shows OFF and behavior is
   *  unchanged from today. */
  enabled: boolean;
  /** A profile is on file (the daemon has an enrolled centroid set). Only when
   *  enrolled does voice-id verify or gate anything. */
  enrolled: boolean;
  /** This turn's verdict: did the speaker match the enrolled owner? Meaningful
   *  only when `enrolled`. A fail-closed turn (no usable audio while enforcing)
   *  reports false. */
  verified: boolean;
  /** Cosine SIMILARITY of this turn's voice to the enrolled profile, in [0,1].
   *  A similarity, NOT a probability and NOT a security guarantee. Null when no
   *  verify verdict has arrived yet (e.g. while only enrollment events have
   *  been seen). */
  score: number | null;
  /** True while an enrollment capture session is open (between
   *  voiceid.enroll_started and voiceid.enrolled). Drives the ENROLLING state. */
  enrolling: boolean;
  /** During enrollment: samples captured so far and how many remain. Both null
   *  outside a session. Surfaced so the indicator can show "2/3 captured". */
  captured: number | null;
  need: number | null;
}

/** The resting voice-id status before any voiceid.* event has been seen: OFF,
 *  not enrolled, no verdict, not enrolling. Used as the reducer seed so the
 *  indicator can render the honest OFF state immediately. */
export function voiceIdInitial(): VoiceIdStatus {
  return {
    enabled: false,
    enrolled: false,
    verified: false,
    score: null,
    enrolling: false,
    captured: null,
    need: null,
  };
}

/** Apply a `voiceid.verify` payload to the prior status. SECRET-FREE: reads
 *  ONLY {verified, score, enabled, enrolled} — any other field on the wire is
 *  ignored. `score` is clamped to [0,1] (a similarity, never < 0 or > 1) and is
 *  null when absent/non-finite. A verify verdict ENDS any enrolling state (the
 *  daemon only emits voiceid.verify on an ordinary turn, not mid-enroll). Never
 *  throws. */
export function applyVoiceIdVerify(
  prev: VoiceIdStatus,
  data: Record<string, unknown>,
): VoiceIdStatus {
  const rawScore = num(data, "score");
  const score =
    rawScore === null ? null : Math.min(1, Math.max(0, rawScore));
  return {
    ...prev,
    enabled: bool(data, "enabled") ?? prev.enabled,
    enrolled: bool(data, "enrolled") ?? prev.enrolled,
    verified: bool(data, "verified") ?? false,
    score,
    // An ordinary verified turn means no capture session is open.
    enrolling: false,
    captured: null,
    need: null,
  };
}

/** Apply a `voiceid.enroll_started` payload: open a capture session. `need`
 *  defaults to null when absent/non-positive. The verdict fields are reset (we
 *  are mid-enroll, there is no current owner verdict). Never throws. */
export function applyVoiceIdEnrollStarted(
  prev: VoiceIdStatus,
  data: Record<string, unknown>,
): VoiceIdStatus {
  const rawNeed = num(data, "need");
  const need = rawNeed !== null && rawNeed > 0 ? Math.floor(rawNeed) : null;
  return {
    ...prev,
    enrolling: true,
    captured: 0,
    need,
    verified: false,
    score: null,
  };
}

/** Apply a `voiceid.enroll_progress` payload: advance the capture counters.
 *  Both default to null when absent/invalid. Never throws. */
export function applyVoiceIdEnrollProgress(
  prev: VoiceIdStatus,
  data: Record<string, unknown>,
): VoiceIdStatus {
  const rawCaptured = num(data, "captured");
  const captured =
    rawCaptured !== null && rawCaptured >= 0 ? Math.floor(rawCaptured) : prev.captured;
  const rawNeed = num(data, "need");
  const need = rawNeed !== null && rawNeed >= 0 ? Math.floor(rawNeed) : prev.need;
  return { ...prev, enrolling: true, captured, need };
}

/** Apply a `voiceid.enrolled` payload: a profile is now on file. Closes the
 *  capture session. We do NOT fabricate a verify verdict — the next ordinary
 *  utterance's voiceid.verify supplies it; until then the indicator shows the
 *  resting ENROLLED state. Never throws. */
export function applyVoiceIdEnrolled(prev: VoiceIdStatus): VoiceIdStatus {
  return {
    ...prev,
    enrolled: true,
    enrolling: false,
    captured: null,
    need: null,
    verified: false,
    score: null,
  };
}

/** Apply a `voiceid.forgot` payload: the profile was cleared. The subsystem may
 *  still be enabled, but with nothing enrolled it enforces nothing — the
 *  indicator falls back to NOT ENROLLED. Never throws. */
export function applyVoiceIdForgot(prev: VoiceIdStatus): VoiceIdStatus {
  return {
    ...prev,
    enrolled: false,
    enrolling: false,
    captured: null,
    need: null,
    verified: false,
    score: null,
  };
}

/** The display state of the voice-id indicator, derived PURELY from a
 *  VoiceIdStatus (so it is unit-tested and shared by the StatusBar chip + the
 *  Settings row). Ordered by what dominates the render:
 *   - "off"          — `!enabled`: the subsystem is off; nothing is gated.
 *   - "enrolling"    — a capture session is open (mid "enroll my voice").
 *   - "unenrolled"   — enabled but no profile: nothing to verify against yet.
 *   - "verified"     — enrolled AND this turn's speaker matched the owner.
 *   - "unrecognized" — enrolled AND this turn's speaker did NOT match.
 *   - "enrolled"     — enrolled, no fresh verdict yet (resting state).
 *  Note: a verdict is shown only when a `voiceid.verify` HAS arrived since the
 *  last enroll/forget (score !== null); otherwise we rest on "enrolled" rather
 *  than asserting a stale verified/unrecognized. */
export type VoiceIdDisplay =
  | "off"
  | "enrolling"
  | "unenrolled"
  | "verified"
  | "unrecognized"
  | "enrolled";

/** Derive the indicator display state. PURE. */
export function voiceIdDisplay(v: VoiceIdStatus): VoiceIdDisplay {
  if (!v.enabled) return "off";
  if (v.enrolling) return "enrolling";
  if (!v.enrolled) return "unenrolled";
  // Enrolled: only assert a verdict when a fresh voiceid.verify has landed
  // (score is the proof a verdict exists); otherwise rest on "enrolled".
  if (v.score === null) return "enrolled";
  return v.verified ? "verified" : "unrecognized";
}

/** Short uppercase label for the indicator chip. */
export function voiceIdLabel(d: VoiceIdDisplay): string {
  switch (d) {
    case "off":
      return "OFF";
    case "enrolling":
      return "ENROLLING";
    case "unenrolled":
      return "NOT ENROLLED";
    case "verified":
      return "VERIFIED";
    case "unrecognized":
      return "UNRECOGNIZED";
    case "enrolled":
      return "ENROLLED";
  }
}

/** FUI tone class for the indicator chip: good (verified), bad (unrecognized),
 *  warn (enrolling), idle/dim (off / not-enrolled / resting-enrolled). Kept as a
 *  small vocabulary mirroring the credential pill tones. */
export function voiceIdTone(d: VoiceIdDisplay): "good" | "bad" | "warn" | "idle" {
  switch (d) {
    case "verified":
      return "good";
    case "unrecognized":
      return "bad";
    case "enrolling":
      return "warn";
    case "off":
    case "unenrolled":
    case "enrolled":
      return "idle";
  }
}

/** The similarity score rendered for humans (a percentage, 0–100), or null when
 *  there is no verdict yet. Framed as a SIMILARITY, never a guarantee — callers
 *  must not present it as a probability of identity. */
export function voiceIdSimilarityPct(v: VoiceIdStatus): number | null {
  return v.score === null ? null : Math.round(v.score * 100);
}

/* ------------------------------------------------------------------------ *
 * MODEL TIER — daemon/src/model_tier.rs + router.rs (the model-tier layer).     *
 * The daemon answers each conversation turn through a tier resolver with the    *
 * precedence Override > Auto > Fallback, and a CONSERVATIVE voice-command path   *
 * ("use the powerful model" / "go offline" / "fast mode" / "auto") that installs *
 * a process-global runtime override. It surfaces TWO secret-free system events:  *
 *                                                                                *
 *   - `model.tier`  {tier, reason, manual, intent} — emitted on EVERY answered   *
 *     conversation turn (cloud OR local). `tier` ∈ local|fast|heavy is WHICH      *
 *     model answered; `reason` ∈ override|auto|fallback is WHY; `manual` mirrors  *
 *     reason==override. This is the live per-turn readout the indicator shows.    *
 *   - `model.swap`  {intent, override, manual} — emitted when a model-control     *
 *     voice command lands. `intent` ∈ heavy|fast|local|auto is what was asked;    *
 *     `override` is the tier now pinned (local|fast|heavy) or null for Auto       *
 *     (override cleared -> the config default resumes). `manual` is false only    *
 *     for Auto. This sets the indicator's MANUAL-vs-AUTO mode IMMEDIATELY, before *
 *     the next answered turn re-confirms it via model.tier.                       *
 *                                                                                *
 * The swap is MODEL-ONLY: it changes which model answers and changes NO safety    *
 * gate (the consequential confirmation gate, the [integrations] master switch,    *
 * the owner voice-id gate, and the per-agent allowlist behave identically at      *
 * every tier). The parsers below carry only the wire fields — no field here can   *
 * fabricate authority or capability.                                             *
 *                                                                                *
 * HONESTY (do not over-state on the way to the HUD):                             *
 *   - LOCAL means NO cloud call: the utterance + content stay ON-DEVICE — a REAL  *
 *     privacy benefit (offline / private). The indicator says so plainly.         *
 *   - The on-device model has a genuine CAPABILITY CEILING (it is the resident    *
 *     ~4B; near-deterministic on some tasks). LOCAL is NOT Opus-grade — the copy  *
 *     must never imply local == the heavy cloud model's quality.                  *
 *   - AUTO is a per-turn difficulty HEURISTIC: it can be wrong, and it is         *
 *     overridable + surfaced. The indicator labels it a heuristic, not a promise. *
 *   - FAST / HEAVY are cloud tiers: they need a cloud key + reachability. When the *
 *     cloud is unreachable (or a cloud call errors) the resolver DEGRADES to       *
 *     local — surfaced as reason=fallback, never a silent wrong answer.            *
 * ------------------------------------------------------------------------ */

/** The model tier answering turns (daemon Tier::as_str). Ordered Local < Fast <
 *  Heavy by capability. `local` is the on-device path (no cloud call); `fast` is
 *  the cloud fast model (Haiku); `heavy` is the cloud heavy model (Opus). */
export type ModelTier = "local" | "fast" | "heavy";

/** WHY a tier was chosen (daemon Reason::as_str): `override` = a manual voice
 *  override is in force (MANUAL); `auto` = no override, the difficulty heuristic
 *  picked it; `fallback` = a cloud tier was wanted but the cloud was unreachable /
 *  errored, so it degraded to local (the honest degrade path). */
export type ModelTierReason = "override" | "auto" | "fallback";

/** The set of swap intents the daemon detects (ModelSwapIntent::as_str). `auto`
 *  clears any override; the other three pin a tier. */
export type ModelSwapIntent = "heavy" | "fast" | "local" | "auto";

/** The live model-tier surface the HUD shows. Folds the per-turn `model.tier`
 *  verdict together with the most recent `model.swap` so the indicator can render
 *  WHICH tier (LOCAL/FAST/HEAVY), the MANUAL-vs-AUTO mode, and the REASON
 *  (override/auto/fallback) the moment a swap lands — without waiting for the next
 *  answered turn.
 *
 *  `tier`/`reason` are null until the first telemetry has been seen (a fresh
 *  daemon has not answered a turn yet) so the indicator can render an honest
 *  "awaiting" resting state rather than asserting a tier nothing confirmed.
 *  `manual` is the MANUAL-vs-AUTO mode: true while a voice override is pinned,
 *  false in AUTO (the config default + heuristic). `lastSwap` is the last swap
 *  intent seen (for a transient ack), null before any swap. */
export interface ModelTierStatus {
  /** WHICH model answered the last turn (or is pinned by a swap), null before any
   *  telemetry. */
  tier: ModelTier | null;
  /** WHY (override/auto/fallback) for the last answered turn, null before any
   *  model.tier. A swap sets the mode (`manual`) but leaves `reason` until the
   *  next answered turn confirms it. */
  reason: ModelTierReason | null;
  /** MANUAL (a voice override is pinned) vs AUTO (config default + heuristic). */
  manual: boolean;
  /** The last swap intent seen (heavy/fast/local/auto), for a transient ack; null
   *  before any model.swap. */
  lastSwap: ModelSwapIntent | null;
}

/** The resting model-tier status before any model.tier/model.swap event: no tier
 *  confirmed yet, AUTO mode (the safe default — no override is in force), no swap
 *  seen. Used as the reducer seed so the indicator renders the honest "awaiting"
 *  state immediately. */
export function modelTierInitial(): ModelTierStatus {
  return { tier: null, reason: null, manual: false, lastSwap: null };
}

/** Narrow an untrusted string to a known ModelTier, or null. */
function asModelTier(v: string | null): ModelTier | null {
  return v === "local" || v === "fast" || v === "heavy" ? v : null;
}

/** Narrow an untrusted string to a known ModelTierReason, or null. */
function asModelTierReason(v: string | null): ModelTierReason | null {
  return v === "override" || v === "auto" || v === "fallback" ? v : null;
}

/** Narrow an untrusted string to a known ModelSwapIntent, or null. */
function asModelSwapIntent(v: string | null): ModelSwapIntent | null {
  return v === "heavy" || v === "fast" || v === "local" || v === "auto" ? v : null;
}

/** Apply a `model.tier` payload (emitted per answered turn). Reads ONLY
 *  {tier, reason, manual}; an unknown tier/reason is ignored (the prior value is
 *  kept) so a garbled frame never blanks the indicator. `manual` is taken from the
 *  bool when present, else derived from reason==="override" (the daemon sets both
 *  in lockstep). Never throws. */
export function applyModelTier(
  prev: ModelTierStatus,
  data: Record<string, unknown>,
): ModelTierStatus {
  const tier = asModelTier(str(data, "tier")) ?? prev.tier;
  const reason = asModelTierReason(str(data, "reason")) ?? prev.reason;
  const manual = bool(data, "manual") ?? reason === "override";
  return { ...prev, tier, reason, manual };
}

/** Apply a `model.swap` payload (emitted when a model-control voice command
 *  lands). Reads ONLY {intent, override, manual}. The pinned `override` (a tier
 *  string for a manual pick, or null for Auto -> config default) drives the MODE
 *  immediately: a tier override sets MANUAL and previews that tier; Auto clears to
 *  AUTO and leaves `tier` to be re-confirmed by the next model.tier turn (it does
 *  NOT fabricate a tier). `manual` defaults to intent!=="auto". `lastSwap` records
 *  the intent for a transient ack. Never throws. */
export function applyModelSwap(
  prev: ModelTierStatus,
  data: Record<string, unknown>,
): ModelTierStatus {
  const intent = asModelSwapIntent(str(data, "intent"));
  const override = asModelTier(str(data, "override")); // null on Auto / absent
  const manual = bool(data, "manual") ?? (intent !== null && intent !== "auto");
  return {
    ...prev,
    lastSwap: intent ?? prev.lastSwap,
    manual,
    // A manual override previews its tier at once; Auto leaves the tier to the
    // next answered turn (the swap does not itself answer a turn, so it never
    // fabricates a tier).
    tier: override ?? prev.tier,
    // The reason a swap implies: a pinned override reads as override; Auto leaves
    // the prior reason for the next turn to refresh (it will be auto/fallback).
    reason: override !== null ? "override" : prev.reason,
  };
}

/* --------------------------------------------------------------- voice tier */

/** Which TTS backend voiced the last reply (daemon `Backend::as_str`): `kokoro` =
 *  the ON-DEVICE default (private/offline, the fallback); `elevenlabs` = the
 *  OPTIONAL CLOUD voice tier (premium voices — the spoken text left the device to
 *  be synthesized). */
export type VoiceBackend = "kokoro" | "elevenlabs";

/** The live voice-tier surface the HUD shows. Folds the per-sentence `voice.tier`
 *  telemetry so the indicator can render CLOUD vs ON-DEVICE voice honestly. The
 *  daemon emits this at backend-selection time and it NEVER carries the key or the
 *  voice id — only {backend, agent}.
 *
 *  `backend` is null until the first `voice.tier` is seen (a fresh daemon has not
 *  spoken yet) so the indicator renders an honest "awaiting" resting state. `agent`
 *  is the agent whose voice last spoke (for context), null before any telemetry. */
export interface VoiceTierStatus {
  /** WHICH TTS backend voiced the last reply, null before any telemetry. */
  backend: VoiceBackend | null;
  /** The agent whose voice last spoke (for context), null before any telemetry. */
  agent: string | null;
}

/** The resting voice-tier status before any voice.tier event: no backend
 *  confirmed yet. Used as the reducer seed so the indicator renders the honest
 *  "awaiting" state immediately. */
export function voiceTierInitial(): VoiceTierStatus {
  return { backend: null, agent: null };
}

/** Narrow an untrusted string to a known VoiceBackend, or null. */
function asVoiceBackend(v: string | null): VoiceBackend | null {
  return v === "kokoro" || v === "elevenlabs" ? v : null;
}

/** Apply a `voice.tier` payload (emitted at backend-selection time, per spoken
 *  reply). Reads ONLY {backend, agent}; an unknown backend is ignored (the prior
 *  value is kept) so a garbled frame never blanks the indicator. The payload
 *  carries NO key/voice id by contract — this reducer reads neither. Never throws. */
export function applyVoiceTier(
  prev: VoiceTierStatus,
  data: Record<string, unknown>,
): VoiceTierStatus {
  const backend = asVoiceBackend(str(data, "backend")) ?? prev.backend;
  const agent = str(data, "agent") ?? prev.agent;
  return { ...prev, backend, agent };
}

/** Short uppercase label for the voice-tier indicator: CLOUD VOICE / ON-DEVICE, or
 *  AWAITING before any telemetry. HONEST copy: "CLOUD VOICE" names that synthesis
 *  leaves the device; "ON-DEVICE" names the private/offline default. PURE. */
export function voiceTierLabel(backend: VoiceBackend | null): string {
  switch (backend) {
    case "elevenlabs":
      return "CLOUD VOICE";
    case "kokoro":
      return "ON-DEVICE";
    case null:
      return "AWAITING";
  }
}

/** FUI tone class for the voice-tier dot: cloud voice reads as an active/accent
 *  state, on-device as the calm "good" default, awaiting as dim/idle. PURE. */
export function voiceTierTone(backend: VoiceBackend | null): string {
  switch (backend) {
    case "elevenlabs":
      return "warn"; // CLOUD: amber accent — text leaves the device (worth noticing)
    case "kokoro":
      return "good"; // ON-DEVICE: the private default
    case null:
      return "idle";
  }
}

/** One-line honest description of the voice tier for a tooltip/caption. PURE. */
export function voiceTierDetail(backend: VoiceBackend | null): string {
  switch (backend) {
    case "elevenlabs":
      return "Premium cloud voices (ElevenLabs) — spoken text leaves the device to synthesize.";
    case "kokoro":
      return "On-device Kokoro — private/offline, the default and the fallback.";
    case null:
      return "Awaiting the first spoken reply.";
  }
}

/* ----------------------------------------------------------------- stt tier */

/** Which STT backend transcribed the user's last captured audio (daemon
 *  `SttBackend::as_str`): `whisper` = the ON-DEVICE default (mlx_whisper —
 *  private/offline, AND the fallback on any cloud error); `elevenlabs_scribe` =
 *  the OPTIONAL gated CLOUD-STT tier (the user's VOICE AUDIO left the device to be
 *  transcribed by ElevenLabs Scribe). STT is MORE sensitive than TTS text — it is
 *  the user's actual voice recording, not synthesized output. */
export type SttBackend = "whisper" | "elevenlabs_scribe";

/** The live STT-tier surface the HUD shows. Folds the per-turn `stt.tier`
 *  telemetry so the indicator can render CLOUD STT vs ON-DEVICE STT honestly. The
 *  daemon emits this at backend-selection time and it carries ONLY {backend} — no
 *  key, no transcript, no audio.
 *
 *  `backend` is null until the first `stt.tier` is seen (a fresh daemon has not
 *  transcribed yet) so the indicator renders an honest "awaiting" resting state. */
export interface SttTierStatus {
  /** WHICH STT backend transcribed the last audio, null before any telemetry. */
  backend: SttBackend | null;
}

/** The resting STT-tier status before any stt.tier event: no backend confirmed
 *  yet. Used as the reducer seed so the indicator renders the honest "awaiting"
 *  state immediately. */
export function sttTierInitial(): SttTierStatus {
  return { backend: null };
}

/** Narrow an untrusted string to a known SttBackend, or null. */
function asSttBackend(v: string | null): SttBackend | null {
  return v === "whisper" || v === "elevenlabs_scribe" ? v : null;
}

/** Apply an `stt.tier` payload (emitted at backend-selection time, per
 *  transcribed turn). Reads ONLY {backend}; an unknown backend is ignored (the
 *  prior value is kept) so a garbled frame never blanks the indicator. The payload
 *  carries NO key/transcript/audio by contract — this reducer reads none of them.
 *  Never throws. */
export function applySttTier(
  prev: SttTierStatus,
  data: Record<string, unknown>,
): SttTierStatus {
  const backend = asSttBackend(str(data, "backend")) ?? prev.backend;
  return { ...prev, backend };
}

/** Short uppercase label for the STT-tier indicator: CLOUD STT / ON-DEVICE STT, or
 *  AWAITING before any telemetry. HONEST copy: "CLOUD STT" names that the audio
 *  left the device; "ON-DEVICE STT" names the private/offline whisper default. PURE. */
export function sttTierLabel(backend: SttBackend | null): string {
  switch (backend) {
    case "elevenlabs_scribe":
      return "CLOUD STT";
    case "whisper":
      return "ON-DEVICE STT";
    case null:
      return "AWAITING";
  }
}

/** FUI tone class for the STT-tier dot. Cloud STT reads as an accent worth
 *  noticing (the user's AUDIO left the device — more sensitive than TTS text), so
 *  it is amber; on-device whisper is the calm "good" private default; awaiting is
 *  dim/idle. PURE. */
export function sttTierTone(backend: SttBackend | null): string {
  switch (backend) {
    case "elevenlabs_scribe":
      return "warn"; // CLOUD: amber — the user's voice audio leaves the device
    case "whisper":
      return "good"; // ON-DEVICE: the private/offline default + fallback
    case null:
      return "idle";
  }
}

/** One-line honest description of the STT tier for a tooltip/caption. PURE. STT is
 *  MORE sensitive than TTS: the cloud path uploads the user's VOICE AUDIO (their
 *  actual recording), where the TTS cloud path only sent synthesized text. */
export function sttTierDetail(backend: SttBackend | null): string {
  switch (backend) {
    case "elevenlabs_scribe":
      return "Cloud transcription (ElevenLabs Scribe) — your VOICE AUDIO leaves the device, more sensitive than TTS text.";
    case "whisper":
      return "On-device whisper (mlx_whisper) — private/offline, the default and the fallback on any cloud error.";
    case null:
      return "Awaiting the first transcribed turn.";
  }
}

/* ------------------------------------------------ audio I/O (#30 / #31 / #32) */

/* ------------------------------------------------------------------------ *
 * AUDIO-INPUT surface for the three audio-input features, all SHIPPING OFF / *
 * neutral behind their own config flags and surfaced READ-ONLY here:         *
 *                                                                            *
 *  #30 CONTINUOUS LIVE INTERPRETATION (daemon/src/interpret.rs + audio.rs).  *
 *      `audio` / `interpret.segment_fed` {target, speak} — emitted at the    *
 *      audio.rs VAD-segment site when [interpret].live is ON: the live       *
 *      DEVICE-GATED mic loop just fed a segment into the PURE interpret       *
 *      pipeline. `local` / `interpret.segment` {to, translated:true, spoke}  *
 *      — emitted by interpret_segment ONLY on a REAL translation (never on an *
 *      honest offline degrade). Together they drive a LIVE INTERPRET status:  *
 *      source(auto-detect when unknown) -> target, render-only vs spoken,     *
 *      and the honest "the always-listening loop is DEVICE-GATED (mic)" copy. *
 *                                                                            *
 *  #31 MULTI-SPEAKER DIARIZATION (daemon/src/main.rs + diarize.rs).          *
 *      `local` / `transcript.diarized` {transcript, turns, multi_speaker,    *
 *      backend_can_diarize} — emitted on the transcript path when            *
 *      [voice].diarize is ON. `backend_can_diarize` is HONESTLY false for     *
 *      on-device whisper (no diarization model — single honest stream) and    *
 *      true only for the EL-Scribe backend that actually carries speaker      *
 *      labels. NEVER fabricates a speaker the backend did not report.         *
 *                                                                            *
 *  #32 CUSTOM WAKE-WORD (daemon/src/wake.rs + audio.rs/router.rs).           *
 *      `audio` / `utterance.no_wake` {phrase, path} — emitted when an         *
 *      utterance is DROPPED for lacking the configured wake phrase. The       *
 *      phrase here is the ACTIVE configured wake word (default "jarvis"),     *
 *      surfaced read-only; the `path` (a local wav path) is NOT carried onto  *
 *      the HUD surface (it is not a panel field).                             *
 *                                                                            *
 * SECRET-FREE: only languages / booleans / counts / the wake phrase ride this *
 * surface. The reducer NEVER renders the diarized transcript text, a          *
 * fabricated speaker, a fabricated translation, or the wav path. All three    *
 * are OFF/neutral by default, so before any telemetry the surface rests in    *
 * the honest "interpret OFF / diarization not seen / wake default" state.     *
 * Parsed/folded DEFENSIVELY — a malformed field is dropped and the prior      *
 * honest value kept; nothing here ever throws.                                *
 * ------------------------------------------------------------------------ */

/** The live LIVE-INTERPRET sub-state. `active` flips true the first time a
 *  segment is fed (interpret.segment_fed) — the honest signal the DEVICE-GATED
 *  live mic loop is running. `target`/`source` are the interpret direction (an
 *  empty source = auto-detect, surfaced honestly as "auto-detect", NEVER a
 *  claimed-known source). `spoke` reflects whether the LAST real translation was
 *  also voiced (render-only otherwise). `translations` counts the REAL
 *  translations rendered (interpret.segment with translated:true) — an honest
 *  "0 so far" vs a running count; a degrade emits no segment so it never bumps. */
export interface InterpretLive {
  active: boolean;
  source: string | null; // null/"" => auto-detect
  target: string | null;
  spoke: boolean;
  translations: number;
}

/** The live DIARIZATION sub-state, folded from transcript.diarized. `seen` is
 *  false until the first frame (so the panel shows the honest "not seen" resting
 *  state). `backendCanDiarize` is the GROUND-TRUTH honesty bit: false for
 *  on-device whisper (single honest stream — no diarization model), true only for
 *  the EL-Scribe backend that carries speaker labels. `multiSpeaker` and `turns`
 *  are the last frame's counts. NEVER carries the transcript text or a fabricated
 *  speaker — only the honest counts + the can-diarize signal. */
export interface DiarizationState {
  seen: boolean;
  backendCanDiarize: boolean;
  multiSpeaker: boolean;
  turns: number;
}

/** The ACTIVE WAKE WORD sub-state. `phrase` is the configured wake phrase the
 *  daemon last reported (via utterance.no_wake) — defaults to "jarvis" (today's
 *  behavior). `lastDropped` flips true once an utterance has been dropped for
 *  lacking the wake word (the honest "the gate is live and rejecting" signal).
 *  The wav path is NEVER carried here. */
export interface WakeWordState {
  phrase: string;
  lastDropped: boolean;
}

/** The combined read-only AUDIO-I/O surface (#30/#31/#32). Always present
 *  (seeded with the honest OFF/neutral resting state) so the panel renders
 *  immediately; each sub-state stays honest until its first telemetry frame. */
export interface AudioIoStatus {
  interpret: InterpretLive;
  diarization: DiarizationState;
  wake: WakeWordState;
}

/** The resting AUDIO-I/O status before any telemetry: interpret idle (no segment
 *  fed), diarization not seen (and honestly NOT able to diarize until an
 *  EL-Scribe frame says otherwise), wake on the default "jarvis" phrase with
 *  nothing dropped yet. All three features ship OFF, so this IS the shipped
 *  default the panel renders. */
export function audioIoInitial(): AudioIoStatus {
  return {
    interpret: {
      active: false,
      source: null,
      target: null,
      spoke: false,
      translations: 0,
    },
    diarization: {
      seen: false,
      backendCanDiarize: false,
      multiSpeaker: false,
      turns: 0,
    },
    wake: { phrase: "jarvis", lastDropped: false },
  };
}

/** Fold an `interpret.segment_fed` payload (audio.rs segment site, [interpret].live
 *  ON): the DEVICE-GATED mic loop fed a segment into the pure pipeline. Marks the
 *  interpret surface ACTIVE and records the direction `target` + whether voicing is
 *  requested (`speak`). A missing/blank target keeps the prior target (a garbled
 *  frame never blanks the direction). Never throws. SECRET-FREE — languages +
 *  a bool only; no transcript, no audio. */
export function applyInterpretSegmentFed(
  prev: AudioIoStatus,
  data: Record<string, unknown>,
): AudioIoStatus {
  const target = str(data, "target");
  const speak = bool(data, "speak");
  return {
    ...prev,
    interpret: {
      ...prev.interpret,
      active: true,
      target: target !== null && target.length > 0 ? target : prev.interpret.target,
      spoke: speak ?? prev.interpret.spoke,
    },
  };
}

/** Fold an `interpret.segment` payload (interpret.rs, emitted ONLY on a REAL
 *  translation — translated:true, never on an honest offline degrade). Records the
 *  resolved target `to`, whether it was voiced (`spoke`), bumps the real-translation
 *  count, and marks the surface active. We only count a frame that honestly carries
 *  translated===true (a degrade emits no segment, but if a malformed frame arrives
 *  without translated:true we do NOT count it as a translation). Never throws.
 *  SECRET-FREE — never the translated text, only the language + booleans. */
export function applyInterpretSegment(
  prev: AudioIoStatus,
  data: Record<string, unknown>,
): AudioIoStatus {
  const translated = bool(data, "translated") === true;
  const to = str(data, "to");
  const spoke = bool(data, "spoke");
  return {
    ...prev,
    interpret: {
      ...prev.interpret,
      active: true,
      target: to !== null && to.length > 0 ? to : prev.interpret.target,
      spoke: spoke ?? prev.interpret.spoke,
      // Only a frame that honestly reports translated:true bumps the real count.
      translations: translated
        ? prev.interpret.translations + 1
        : prev.interpret.translations,
    },
  };
}

/** Fold a `transcript.diarized` payload (main.rs transcript path, [voice].diarize
 *  ON). Records the GROUND-TRUTH `backend_can_diarize` honesty bit (false on
 *  on-device whisper — single honest stream; true on EL Scribe), the `multi_speaker`
 *  flag, and the `turns` count. Marks the surface SEEN. NEVER reads the `transcript`
 *  text — the diarized transcript is rendered in the comms panel, not here, and a
 *  fabricated speaker is never surfaced (an on-device frame reads honestly as a
 *  single stream). Defaults are conservative (can-diarize false, not multi) so a
 *  garbled frame can never over-claim. Never throws. */
export function applyTranscriptDiarized(
  prev: AudioIoStatus,
  data: Record<string, unknown>,
): AudioIoStatus {
  const backendCanDiarize = bool(data, "backend_can_diarize") ?? false;
  // multi_speaker can only honestly be true when the backend can actually diarize;
  // on-device whisper (can_diarize false) is a single stream by construction, so we
  // never surface a fabricated multi-speaker claim it could not have produced.
  const multiSpeaker = backendCanDiarize && (bool(data, "multi_speaker") ?? false);
  return {
    ...prev,
    diarization: {
      seen: true,
      backendCanDiarize,
      multiSpeaker,
      turns: nonNegInt(data, "turns"),
    },
  };
}

/** Fold an `utterance.no_wake` payload (audio.rs/router.rs, wake gate dropped an
 *  utterance for lacking the phrase). Records the ACTIVE configured wake `phrase`
 *  and marks that the gate has dropped something (the honest "the gate is live"
 *  signal). A missing/blank phrase keeps the prior phrase (never blanks the active
 *  wake word). The `path` (a local wav path) is DELIBERATELY not read — it is not a
 *  panel field. Never throws. */
export function applyUtteranceNoWake(
  prev: AudioIoStatus,
  data: Record<string, unknown>,
): AudioIoStatus {
  const phrase = str(data, "phrase");
  return {
    ...prev,
    wake: {
      phrase: phrase !== null && phrase.trim().length > 0 ? phrase : prev.wake.phrase,
      lastDropped: true,
    },
  };
}

/** Short uppercase label for the LIVE-INTERPRET indicator. ACTIVE once the mic
 *  loop has fed a segment (device-gated), else the honest OFF resting state. PURE. */
export function interpretLabel(i: InterpretLive): string {
  return i.active ? "LIVE INTERPRET" : "INTERPRET OFF";
}

/** Human source->target direction for the interpret readout. An empty/unknown
 *  source reads honestly as "auto-detect" (Babel never claims a source it only
 *  guessed); an empty target reads as "—" (the honest "which language?" state).
 *  PURE. */
export function interpretDirection(i: InterpretLive): string {
  const src = i.source !== null && i.source.length > 0 ? i.source : "auto-detect";
  const tgt = i.target !== null && i.target.length > 0 ? i.target : "—";
  return `${src} → ${tgt}`;
}

/** FUI tone for the interpret dot: active reads as an accent worth noticing
 *  (the always-listening loop is engaged — amber), idle is the calm OFF default.
 *  PURE. */
export function interpretTone(i: InterpretLive): string {
  return i.active ? "warn" : "idle";
}

/** Short uppercase label for the DIARIZATION indicator: MULTI-SPEAKER (EL Scribe
 *  reported >1 distinct speaker), SINGLE STREAM (EL Scribe present but one
 *  speaker), ON-DEVICE: NO DIARIZATION (the honest on-device whisper state — no
 *  model, single honest stream), or NOT SEEN before the first frame. PURE. */
export function diarizationLabel(d: DiarizationState): string {
  if (!d.seen) return "NOT SEEN";
  if (!d.backendCanDiarize) return "ON-DEVICE: NO DIARIZATION";
  return d.multiSpeaker ? "MULTI-SPEAKER" : "SINGLE STREAM";
}

/** FUI tone for the diarization dot. A confirmed multi-speaker diarization is an
 *  accent (amber — distinct labelled speakers); a single stream / on-device / not
 *  seen are the calm states. PURE. */
export function diarizationTone(d: DiarizationState): string {
  if (!d.seen) return "idle";
  return d.backendCanDiarize && d.multiSpeaker ? "warn" : "good";
}

/** One-line honest description of the diarization posture for a caption/tooltip.
 *  Names that diarization is ElevenLabs-Scribe-ONLY: on-device whisper has no
 *  diarization model and is an honest single stream — never a fabricated speaker.
 *  PURE. */
export function diarizationDetail(d: DiarizationState): string {
  if (!d.seen) {
    return "No diarized transcript yet. Diarization is ElevenLabs-Scribe-only ([voice].diarize ships OFF); on-device whisper has no diarization model and reads as a single honest stream — never a fabricated speaker.";
  }
  if (!d.backendCanDiarize) {
    return "On-device whisper has NO diarization model — this is a single honest stream (speaker: unknown), never a fabricated speaker. Speaker labels need the ElevenLabs-Scribe backend, which carries them.";
  }
  return d.multiSpeaker
    ? "ElevenLabs Scribe reported MULTIPLE distinct speakers — the labels are the backend's, never fabricated by JARVIS."
    : "ElevenLabs Scribe is active but reported a single speaker this turn — an honest single stream.";
}

/* ------------------------------------------------------- voice mode (prosody) */

/* ------------------------------------------------------------------------ *
 * ADAPTIVE TONE / WHISPER (daemon/src/prosody.rs `emit_telemetry`). The      *
 * EXPRESSIVENESS surface for #33 (adaptive prosody) + #34 (whisper/discreet  *
 * mode). The daemon emits `voice.prosody` on the "voice" source at speak     *
 * time, carrying ONLY non-secret DELIVERY facts (mirrors the voice.tier /    *
 * stt.tier discipline) — NEVER the key, the voice id, or the spoken text:    *
 *   { profile (neutral|calm|urgent|warm), backend (kokoro|elevenlabs),       *
 *     rich (bool — the honest "EL-v3 rich prosody actually applied" bit),    *
 *     whisper (bool), terse (bool), rate (f32), volume (f32) }.              *
 *                                                                            *
 * HONESTY (the whole point, surfaced verbatim by the chip):                  *
 *   - Rich prosody is EL-v3-GATED. The `rich` bit is the GROUND TRUTH that   *
 *     audio-tags + stability/style were really applied — true ONLY on        *
 *     ElevenLabs v3. On Kokoro / a non-v3 EL model the daemon sends a COARSE *
 *     rate-only mapping and `rich:false`; the chip says so rather than       *
 *     implying local prosody is rich.                                        *
 *   - WHISPER changes DELIVERY only (terser + softer); it NEVER suppresses a *
 *     required safety confirmation — a required confirm still speaks, just   *
 *     softly/tersely is NOT applied to it (the daemon keeps it full-volume). *
 *     The chip's copy states this; it is a read-only indicator and toggles   *
 *     no gate.                                                               *
 *                                                                            *
 * Both features ship OFF/neutral by default — the resting indicator (before  *
 * any telemetry, or a malformed frame) reads the HONEST default: profile     *
 * NEUTRAL, no rich prosody, whisper OFF. Parsed DEFENSIVELY: unknown profile *
 * falls back to neutral, non-finite rate/volume default to 1.0, junk yields  *
 * the resting state — never a throw, never a fabricated "rich" claim.        *
 * ------------------------------------------------------------------------ */

/** The tone profile colouring the last spoken reply (daemon
 *  `ProsodyProfile::as_str`). `neutral` is the default + the only profile while
 *  #33 is OFF. A future/unknown profile string is narrowed back to `neutral` so
 *  the indicator never shows a tone the daemon did not actually mean. */
export type ProsodyProfile = "neutral" | "calm" | "urgent" | "warm";

/** The live voice-mode (prosody + whisper) surface the HUD shows. Folds the
 *  per-reply `voice.prosody` telemetry so the indicator can render the current
 *  TONE, whether RICH prosody was honestly applied (EL-v3 only), and the WHISPER
 *  state. Carries ONLY non-secret delivery facts — never a key/voice id/text.
 *
 *  `seen` is false until the first `voice.prosody` frame arrives (a fresh daemon
 *  has not spoken yet) so the indicator can render an honest resting default
 *  rather than implying a tone was chosen. */
export interface VoiceModeStatus {
  /** The tone profile of the last spoken reply (neutral while #33 is OFF). */
  profile: ProsodyProfile;
  /** Which TTS backend voiced it — gates whether rich prosody is even possible. */
  backend: VoiceBackend | null;
  /** GROUND TRUTH: whether the rich EL-v3 surface (audio-tags + stability/style)
   *  was ACTUALLY applied. Only ever true on ElevenLabs v3 — false on Kokoro /
   *  non-v3 EL / when #33 is off. Never fabricated. */
  rich: boolean;
  /** Whether WHISPER / discreet mode is currently engaged (delivery only). */
  whisper: boolean;
  /** Whether the last reply was made TERSE by whisper mode. */
  terse: boolean;
  /** The coarse delivery rate multiplier (1.0 = neutral). */
  rate: number;
  /** The coarse output volume multiplier (1.0 = full; whisper lowers it). */
  volume: number;
  /** False until the first voice.prosody frame — drives the resting default. */
  seen: boolean;
}

/** The resting voice-mode status before any voice.prosody event: the HONEST
 *  default both features ship at — profile NEUTRAL, no rich prosody, whisper OFF,
 *  neutral rate/volume. Used as the reducer seed so the indicator renders the
 *  honest default immediately rather than a blank. */
export function voiceModeInitial(): VoiceModeStatus {
  return {
    profile: "neutral",
    backend: null,
    rich: false,
    whisper: false,
    terse: false,
    rate: 1.0,
    volume: 1.0,
    seen: false,
  };
}

/** Narrow an untrusted string to a known ProsodyProfile, defaulting to `neutral`
 *  (the honest, conservative default) for anything else — an unknown/garbled
 *  profile must never surface as a tone the daemon did not pick. */
function asProsodyProfile(v: string | null): ProsodyProfile {
  return v === "calm" || v === "urgent" || v === "warm" ? v : "neutral";
}

/** Apply a `voice.prosody` payload (emitted at speak time, per spoken reply).
 *  Reads ONLY the contracted non-secret delivery fields; an unknown profile falls
 *  back to neutral and a non-finite rate/volume defaults to 1.0, so a garbled
 *  frame degrades to the honest default rather than blanking or fabricating.
 *  `rich` is read straight off the wire (the daemon's ground-truth bit) but is
 *  pinned false unless the backend is ElevenLabs — rich prosody is EL-gated, so a
 *  payload claiming rich:true on Kokoro is never honoured (defence in depth on the
 *  honesty contract). The payload carries NO key/voice id/text by contract — this
 *  reducer reads none of them. Never throws. */
export function applyVoiceMode(
  prev: VoiceModeStatus,
  data: Record<string, unknown>,
): VoiceModeStatus {
  const backend = asVoiceBackend(str(data, "backend")) ?? prev.backend;
  // rich is EL-v3-gated: never honour a rich:true claim on a non-EL backend.
  const richClaim = bool(data, "rich") ?? false;
  const rich = backend === "elevenlabs" ? richClaim : false;
  return {
    profile: asProsodyProfile(str(data, "profile")),
    backend,
    rich,
    whisper: bool(data, "whisper") ?? false,
    terse: bool(data, "terse") ?? false,
    rate: num(data, "rate") ?? 1.0,
    volume: num(data, "volume") ?? 1.0,
    seen: true,
  };
}

/** Short uppercase tone label for the voice-mode chip: NEUTRAL / CALM / URGENT /
 *  WARM. PURE. */
export function prosodyProfileLabel(profile: ProsodyProfile): string {
  return profile.toUpperCase();
}

/** FUI tone class for the voice-mode dot, keyed off the prosody profile. Urgent
 *  reads as the attention accent (amber); calm/warm as the calm "good" default;
 *  neutral as the idle/dim resting look (NOT a verdict — just the plain default).
 *  PURE. */
export function voiceModeTone(profile: ProsodyProfile): string {
  switch (profile) {
    case "urgent":
      return "warn";
    case "calm":
    case "warm":
      return "good";
    case "neutral":
      return "idle";
  }
}

/** Honest one-line description of whether RICH prosody is active — the whole
 *  point of the indicator. States the EL-v3 gate verbatim: rich audio-tags +
 *  stability/style are real ONLY on ElevenLabs v3; on the on-device default (and
 *  any non-v3 EL model) the daemon applies a COARSE rate-only mapping, never faked
 *  rich prosody. PURE. */
export function voiceModeRichDetail(v: VoiceModeStatus): string {
  if (v.rich) {
    return "Rich prosody ACTIVE — ElevenLabs v3 audio-tags + stability/style applied.";
  }
  if (v.backend === "kokoro") {
    return "Rich prosody OFF — on-device Kokoro gets a coarse rate-only mapping; rich prosody is ElevenLabs v3 only, never faked locally.";
  }
  if (v.backend === "elevenlabs") {
    return "Rich prosody OFF — this ElevenLabs model is not v3; only the v3 model carries audio-tags + stability/style.";
  }
  return "Rich prosody OFF — backend-gated to ElevenLabs v3; local gets a coarse/neutral mapping.";
}

/** Honest one-line description of the WHISPER state for a tooltip/caption. States
 *  the never-silence guarantee: whisper changes DELIVERY (terser + softer), it
 *  never suppresses a required safety confirmation. PURE. */
export function voiceModeWhisperDetail(v: VoiceModeStatus): string {
  return v.whisper
    ? "Whisper mode ON — replies are terser + softer (delivery only). A required confirmation still speaks fully; whisper never suppresses a safety gate."
    : "Whisper mode OFF — normal delivery. (Whisper changes delivery only, never whether a required confirmation speaks.)";
}

/** Short uppercase label for the indicator: LOCAL / FAST / HEAVY, or AWAITING
 *  before any telemetry. PURE. */
export function modelTierLabel(tier: ModelTier | null): string {
  switch (tier) {
    case "local":
      return "LOCAL";
    case "fast":
      return "FAST";
    case "heavy":
      return "HEAVY";
    case null:
      return "AWAITING";
  }
}

/** FUI tone class for the indicator, mirroring the credential/voice-id vocabulary.
 *  heavy = good (most capable cloud), fast = warn-ish "ok" cyan, local = idle
 *  (on-device/dim — NOT a quality verdict, just a muted on-device look), a
 *  `fallback` reason flags bad (a degrade the user should notice). Awaiting = idle.
 *  PURE. */
export function modelTierTone(
  tier: ModelTier | null,
  reason: ModelTierReason | null,
): "good" | "bad" | "warn" | "ok" | "idle" {
  if (reason === "fallback") return "bad"; // a degrade — surface it
  switch (tier) {
    case "heavy":
      return "good";
    case "fast":
      return "warn";
    case "local":
      return "idle";
    case null:
      return "idle";
  }
}

/** The MANUAL-vs-AUTO mode word for the indicator. PURE. */
export function modelTierModeLabel(manual: boolean): "MANUAL" | "AUTO" {
  return manual ? "MANUAL" : "AUTO";
}

/** Short uppercase reason label: OVERRIDE / AUTO / FALLBACK, or empty before any
 *  reason is known. PURE. */
export function modelTierReasonLabel(reason: ModelTierReason | null): string {
  switch (reason) {
    case "override":
      return "OVERRIDE";
    case "auto":
      return "AUTO";
    case "fallback":
      return "FALLBACK";
    case null:
      return "";
  }
}

/** An HONEST one-line description of a tier for the hover/Settings copy. LOCAL is
 *  named as the on-device PRIVACY path WITH its capability ceiling stated — never
 *  dressed up as cloud-grade. FAST/HEAVY are named as cloud tiers (key required).
 *  PURE. Mirrors the daemon Tier::honest_label but is the HUD's own copy. */
export function modelTierHonest(tier: ModelTier | null): string {
  switch (tier) {
    case "local":
      return "on-device — no cloud call (private), but capability-limited (not Opus-grade)";
    case "fast":
      return "cloud fast model — quick + cheap (needs a cloud key)";
    case "heavy":
      return "cloud heavy model — most capable (needs a cloud key)";
    case null:
      return "awaiting the first answered turn";
  }
}

/** An HONEST one-line gloss on the reason for the hover copy. PURE. */
export function modelTierReasonHonest(reason: ModelTierReason | null): string {
  switch (reason) {
    case "override":
      return "you pinned this tier by voice (MANUAL) — it overrides the auto pick";
    case "auto":
      return "picked automatically per turn by a difficulty HEURISTIC (can be wrong; overridable)";
    case "fallback":
      return "a cloud tier was wanted but the cloud was unreachable — degraded to on-device (no cloud call)";
    case null:
      return "";
  }
}

/* ------------------------------------------------------------------------ *
 * RESIDENT LOCAL MODELS — daemon/src/model_tier.rs::local_warm_telemetry (task   *
 * #17, item 3). The daemon emits ONE secret-free startup-snapshot event the HUD  *
 * folds into the RESIDENT-MODELS indicator:                                      *
 *                                                                                *
 *   - `model.local_warm` {base, planned, multi_resident, budget_gib} — the       *
 *     CONFIG-DERIVED warm-set PLAN for the Local tier. `base` is the always-      *
 *     resident [models].llm (the single-resident default + the safe fallback);   *
 *     `planned` is the budget-bounded warm-set the policy ADMITS (base first);    *
 *     `multi_resident` is true iff planned.len()>1 (an instant local swap is      *
 *     possible WHEN RAM allows); `budget_gib` is the configured RAM budget        *
 *     (0 => single-resident). It mirrors server.py's                              *
 *     `InferenceEngine.local_warm_status` but is built PURELY from config — the   *
 *     daemon knows the PLAN, only the server knows what is actually resident.     *
 *                                                                                *
 *   - the per-turn `model.tier` payload may carry `local_sub` ∈ fast|capable|auto *
 *     (daemon LocalSubTier::as_str) — which warm local model answered THIS turn   *
 *     when multi-resident. Folded as the ACTIVE sub-choice for the card.          *
 *                                                                                *
 * HONESTY (carried verbatim to the HUD copy — do NOT over-state):                *
 *   - Multi-resident keeps >1 local model WARM for an INSTANT local swap ONLY     *
 *     WHEN RAM allows: it is heavily RAM/device-gated (two models ~2x RAM).       *
 *   - The DEFAULT is single-resident (multi_resident=false) — the safe behavior   *
 *     on a low-RAM Mac (e.g. an 8GB M1), unchanged from today.                    *
 *   - This reports the PLAN, NOT a measured speed benefit: the swap benefit is    *
 *     device/RAM-dependent and is NOT claimed measured here.                      *
 *   - It changes NO safety gate and does NOT change which TIER is chosen — it      *
 *     only refines the already-chosen Local tier (which warm local model answers).*
 * ------------------------------------------------------------------------ */

/** The daemon's `model.local_warm` startup-snapshot event name. */
export const MODEL_LOCAL_WARM_EVENT = "model.local_warm";

/** The Local-tier SUB-CHOICE label (daemon `LocalSubTier::as_str`): `fast` = the
 *  small local-fast warm model; `capable` = the base [models].llm; `auto` = picked
 *  per turn by difficulty. Folded from the per-turn `model.tier`'s `local_sub`
 *  field, present only when multi-resident actually chose a non-default sub-model. */
export type LocalSubTier = "fast" | "capable" | "auto";

/** The live RESIDENT-MODELS surface the HUD shows. Folds the config-derived
 *  `model.local_warm` PLAN (which local models the policy keeps warm + whether
 *  multi-resident is in effect) with the per-turn `local_sub` ACTIVE sub-choice.
 *  Built PURELY from secret-free wire fields.
 *
 *  `base` is null until the first `model.local_warm` snapshot arrives (a fresh
 *  daemon has not reported) so the indicator can render an honest "awaiting" /
 *  resting state. `multiResident=false` is the safe single-resident low-RAM
 *  default. This is the PLAN — NOT a measured speed benefit (the swap benefit is
 *  device/RAM-gated and is not measured). */
export interface LocalWarmStatus {
  /** The always-resident base/primary local model ([models].llm), null before the
   *  first snapshot. */
  base: string | null;
  /** The ordered warm-set the policy admits under the RAM budget (base first);
   *  empty before the first snapshot. */
  planned: string[];
  /** True iff the policy admitted >1 model (an instant local swap is possible WHEN
   *  RAM allows). False => single-resident (the safe low-RAM default). */
  multiResident: boolean;
  /** The configured RAM budget (GiB); 0 => single-resident. */
  budgetGib: number;
  /** The ACTIVE Local sub-choice this turn (fast/capable/auto), null before any
   *  multi-resident turn reported a `local_sub`. */
  activeSub: LocalSubTier | null;
}

/** The resting resident-models status before any `model.local_warm` snapshot: no
 *  base confirmed, single-resident (the safe default), no active sub-choice. Used
 *  as the reducer seed so the indicator renders the honest "awaiting" state. */
export function localWarmInitial(): LocalWarmStatus {
  return { base: null, planned: [], multiResident: false, budgetGib: 0, activeSub: null };
}

/** Narrow an untrusted string to a known LocalSubTier, or null. */
export function asLocalSubTier(v: string | null): LocalSubTier | null {
  return v === "fast" || v === "capable" || v === "auto" ? v : null;
}

/** Apply a `model.local_warm` snapshot (config-derived, emitted at startup). Reads
 *  ONLY {base, planned, multi_resident, budget_gib}; a missing/garbled field keeps
 *  the prior value so a bad frame never blanks the indicator. `multiResident` is
 *  taken from the bool when present, else derived from planned.length>1 (the daemon
 *  sets both in lockstep). The `activeSub` is left untouched (it comes from the
 *  per-turn model.tier). Never throws. */
export function applyLocalWarm(
  prev: LocalWarmStatus,
  data: Record<string, unknown>,
): LocalWarmStatus {
  const base = str(data, "base") ?? prev.base;
  const planned = strArr(data, "planned") ?? prev.planned;
  const budgetGib = num(data, "budget_gib") ?? prev.budgetGib;
  const multiResident = bool(data, "multi_resident") ?? planned.length > 1;
  return { ...prev, base, planned, budgetGib, multiResident };
}

/** Fold the per-turn `model.tier` payload's optional `local_sub` into the resident-
 *  models surface — the ACTIVE warm local model sub-choice this on-device turn. An
 *  absent/unknown value keeps the prior active sub. Never throws. */
export function applyLocalSub(
  prev: LocalWarmStatus,
  data: Record<string, unknown>,
): LocalWarmStatus {
  const activeSub = asLocalSubTier(str(data, "local_sub")) ?? prev.activeSub;
  return { ...prev, activeSub };
}

/** Short uppercase label for the resident-models indicator: SINGLE / MULTI, or
 *  AWAITING before the first snapshot. HONEST: "MULTI" names that >1 local model is
 *  kept warm (instant swap WHEN RAM allows); "SINGLE" names the safe single-
 *  resident low-RAM default. PURE. */
export function localWarmLabel(s: LocalWarmStatus): string {
  if (s.base === null) return "AWAITING";
  return s.multiResident ? "MULTI" : "SINGLE";
}

/** FUI tone class for the resident-models dot. Multi-resident reads as an active
 *  accent worth noticing (an extra capability that costs RAM), single-resident is
 *  the calm "good" private default, awaiting is dim/idle. It is a PLAN, never a
 *  speed verdict. PURE. */
export function localWarmTone(s: LocalWarmStatus): "good" | "warn" | "idle" {
  if (s.base === null) return "idle";
  return s.multiResident ? "warn" : "good";
}

/** The number of EXTRA warm local models beyond the base (>=0). PURE. */
export function localWarmExtraCount(s: LocalWarmStatus): number {
  return Math.max(0, s.planned.length - 1);
}

/** Short uppercase label for the active Local sub-choice (FAST/CAPABLE/AUTO), or
 *  empty before any. PURE. */
export function localSubLabel(sub: LocalSubTier | null): string {
  switch (sub) {
    case "fast":
      return "FAST";
    case "capable":
      return "CAPABLE";
    case "auto":
      return "AUTO";
    case null:
      return "";
  }
}

/** An HONEST one-line description of the resident-models plan for the hover copy.
 *  States that MULTI keeps >1 local model warm for instant swap ONLY when RAM
 *  allows (~2x RAM), that SINGLE is the safe low-RAM default, and that the speed
 *  benefit is device-dependent and NOT measured (this is the PLAN). PURE. */
export function localWarmHonest(s: LocalWarmStatus): string {
  if (s.base === null) {
    return "Awaiting the resident-models plan from the daemon.";
  }
  if (s.multiResident) {
    const extra = localWarmExtraCount(s);
    return (
      `Multi-resident: keeps ${extra} extra local model${extra === 1 ? "" : "s"} warm ` +
      "alongside the base for an INSTANT local swap — ONLY when RAM allows (two " +
      "models ~2x RAM). This is the PLANNED warm-set under the RAM budget, NOT a " +
      "measured speed benefit (the swap benefit is device/RAM-dependent and not " +
      "measured). Single-resident is the safe default on a low-RAM Mac. It changes " +
      "NO safety gate and does NOT change which tier is chosen — it only refines " +
      "which warm local model answers an on-device turn."
    );
  }
  return (
    "Single-resident: one local model is kept warm (the safe default, unchanged on " +
    "a low-RAM Mac). Multi-resident would keep >1 local model warm for an instant " +
    "swap WHEN RAM allows (~2x RAM), but it is OFF here. The speed benefit is " +
    "device-dependent and not measured."
  );
}

/* ------------------------------------------------------------------------ *
 * INFERENCE PERF — speculative decoding (#37), battery/thermal throttle (#38), *
 * selectable quantization (#39). The daemon folds three PER-TURN, secret-free   *
 * inference facts into the existing `model.tier` payload (router.rs:747 / :1341 *
 * + the server's generate-op response, whose extra fields the daemon's Response *
 * struct ignores — no deny_unknown_fields, so they are backward-compatible):    *
 *                                                                               *
 *   - `speculative` (bool) — the path that ACTUALLY ran this turn. The server   *
 *     returns `speculative=true` ONLY when a draft model was configured AND      *
 *     loadable AND speculative generation actually drove the decode; otherwise   *
 *     `false` (normal generation). It NEVER fakes speculative — a missing/       *
 *     unloadable draft honestly falls back to normal gen and reports false.      *
 *   - `quant` (string) — the quant that ACTUALLY loaded (auto/fp16/int8/int4).   *
 *     If the requested quant variant wasn't present on disk the server falls     *
 *     back to the available one and reports THAT — it never claims int4 when     *
 *     fp16 loaded. `auto` means "loaded the model as configured" (today's        *
 *     behavior).                                                                 *
 *   - `throttle` {reason, tier_pref, defer_heavy} — present ONLY on a LOCAL turn *
 *     when the plan ACTUALLY throttles. Under the OFF default ([power].adaptive  *
 *     off) the plan is neutral and NO throttle field is emitted — so the HUD     *
 *     shows no throttle indicator, never a phantom one. `reason` ∈               *
 *     disabled/nominal/low_battery/thermal (ThrottleReason::as_str); `tier_pref` *
 *     ∈ fast/capable/auto (LocalSubTier::as_str).                                *
 *                                                                               *
 * HONESTY (carried verbatim to the HUD copy — do NOT over-state):               *
 *   - The REAL speedup (speculative), RAM/quality tradeoff (quant), and thermal/ *
 *     battery effect (throttle) are DEVICE/MODEL-GATED — they are never measured *
 *     or claimed headlessly. The panel reports only the PATH THAT ACTUALLY RAN,  *
 *     never a fabricated perf number.                                            *
 *   - All three ship OFF/neutral: speculative=false (normal generation), quant=  *
 *     auto (load as configured), [power].adaptive off (no power read, no         *
 *     throttle). OFF => today's exact runtime behavior.                          *
 *   - The live power read (pmset / thermal pressure / IOKit) happens ONLY when   *
 *     [power].adaptive is on (device-gated). The panel says so plainly.          *
 * ------------------------------------------------------------------------ */

/** Why the daemon's throttle plan acted this turn (ThrottleReason::as_str).
 *  `disabled` = [power].adaptive off (the OFF default — never reaches the HUD as
 *  a throttle since a neutral plan emits no field); `nominal` = on AC + nominal
 *  thermal (no throttle); `low_battery` = low battery and not on AC; `thermal` =
 *  serious/critical thermal pressure. */
export type ThrottleReason = "disabled" | "nominal" | "low_battery" | "thermal";

/** The Local sub-tier the throttle plan prefers (LocalSubTier::as_str): `fast`
 *  (prefer the small warm model — the throttled choice), `capable`, or `auto`
 *  (no preference, the neutral default). */
export type ThrottleTierPref = "fast" | "capable" | "auto";

/** The live throttle plan the daemon emitted on a LOCAL turn — present ONLY when
 *  the plan ACTUALLY throttled (a neutral/OFF plan emits no field, so this is null
 *  on the HUD). Mirrors daemon ThrottlePlan {reason, tier_pref, defer_heavy}. */
export interface ThrottlePlan {
  /** WHY the plan throttled (low_battery/thermal). Never `disabled`/`nominal`
   *  here — the daemon omits the field unless the plan actually throttled. */
  reason: ThrottleReason;
  /** The Local sub-tier the plan prefers under pressure (fast = the throttled
   *  pick). */
  tierPref: ThrottleTierPref;
  /** True iff the plan defers heavy work (e.g. speculative's extra draft pass)
   *  this turn. */
  deferHeavy: boolean;
}

/** The live INFERENCE-PERF surface the HUD shows. Folds the three per-turn
 *  inference facts the daemon carries on `model.tier`:
 *    - `speculative` — whether speculative decoding ACTUALLY ran this turn;
 *    - `quant` — the quant that ACTUALLY loaded;
 *    - `throttle` — the active throttle plan, or null when nothing throttled.
 *
 *  `speculative`/`quant` are null until the first answered turn reports them (a
 *  fresh daemon, or an old server that doesn't send them — the honest "awaiting"
 *  resting state, NOT an assertion). `throttle` is null whenever the last turn
 *  carried no throttle field (the OFF/neutral default — honest, never a phantom).
 *  This reports only the PATH THAT ACTUALLY RAN — never a measured perf number. */
export interface InferencePerfStatus {
  /** Whether speculative decoding ACTUALLY drove the decode last turn; null
   *  before any turn reported it (awaiting). Never true unless a draft model was
   *  loadable AND speculative generation really ran. */
  speculative: boolean | null;
  /** The quant that ACTUALLY loaded last turn (auto/fp16/int8/int4); null before
   *  any turn reported it. `auto` = loaded as configured (today's behavior). */
  quant: string | null;
  /** The active throttle plan, or null when the last turn carried no throttle
   *  (OFF/neutral default — honest, never a phantom indicator). */
  throttle: ThrottlePlan | null;
}

/** The resting inference-perf status before any answered turn: nothing reported
 *  yet (awaiting), no throttle (the OFF/neutral default). Used as the reducer seed
 *  so the panel renders the honest resting state. */
export function inferencePerfInitial(): InferencePerfStatus {
  return { speculative: null, quant: null, throttle: null };
}

/** Narrow an untrusted string to a known ThrottleReason, or null. */
function asThrottleReason(v: string | null): ThrottleReason | null {
  return v === "disabled" || v === "nominal" || v === "low_battery" || v === "thermal"
    ? v
    : null;
}

/** Narrow an untrusted string to a known ThrottleTierPref, or null. */
function asThrottleTierPref(v: string | null): ThrottleTierPref | null {
  return v === "fast" || v === "capable" || v === "auto" ? v : null;
}

/** The quant strings the panel will render as a known badge tone (mirrors the
 *  server's ALLOWED_QUANT + the daemon's quant_is_valid). An unknown string is
 *  still SHOWN verbatim (honest — it is the quant the server reported) but reads
 *  as a neutral tone. */
const KNOWN_QUANTS = new Set(["auto", "fp16", "int8", "int4"]);

/** Parse a `model.tier` payload's optional nested `throttle` object into a
 *  ThrottlePlan, or null. Returns null when the field is ABSENT (the OFF/neutral
 *  default — no throttle) OR malformed (a garbled object drops to the honest
 *  "no throttle" rather than inventing one). A throttle with an unknown reason
 *  is dropped (we never label a throttle we can't name). Never throws. */
function parseThrottle(data: Record<string, unknown>): ThrottlePlan | null {
  const raw = data["throttle"];
  if (!isPlainObject(raw)) return null;
  const reason = asThrottleReason(str(raw, "reason"));
  // The daemon only emits `throttle` when the plan ACTUALLY throttled, so a
  // real field never carries disabled/nominal. Drop an unknown reason (and the
  // neutral reasons) — we never render a throttle we can't honestly name.
  if (reason === null || reason === "disabled" || reason === "nominal") return null;
  const tierPref = asThrottleTierPref(str(raw, "tier_pref")) ?? "auto";
  const deferHeavy = bool(raw, "defer_heavy") ?? false;
  return { reason, tierPref, deferHeavy };
}

/** Fold a per-turn `model.tier` payload's optional inference-perf fields into the
 *  surface. Reads ONLY {speculative, quant, throttle}:
 *    - `speculative` keeps the prior value when absent (an old server / a turn
 *      that didn't report it never blanks a known value) — but a present bool is
 *      taken verbatim (the path that actually ran);
 *    - `quant` keeps the prior value when absent; a present string is taken
 *      verbatim (the quant that actually loaded), even an unknown one (it is what
 *      the server honestly reported);
 *    - `throttle` is REPLACED every turn (it is a live per-turn plan): a present
 *      well-formed throttle sets the plan, an ABSENT field clears it to null (the
 *      OFF/neutral default — no phantom throttle lingers from a prior turn).
 *  Never throws. */
export function applyInferencePerf(
  prev: InferencePerfStatus,
  data: Record<string, unknown>,
): InferencePerfStatus {
  const speculative = bool(data, "speculative") ?? prev.speculative;
  const quant = str(data, "quant") ?? prev.quant;
  // The throttle is a LIVE per-turn plan: replace it every turn so a stale
  // throttle never lingers. Absent => null (the OFF/neutral default).
  const throttle = parseThrottle(data);
  return { speculative, quant, throttle };
}

/** Short uppercase label for the speculative-decoding state: ON (it actually ran
 *  this turn), OFF (normal generation ran), or AWAITING before any turn reported.
 *  HONEST: ON means speculative generation ACTUALLY drove the decode — never the
 *  mere config flag. PURE. */
export function speculativeLabel(s: InferencePerfStatus): string {
  if (s.speculative === null) return "AWAITING";
  return s.speculative ? "ON" : "OFF";
}

/** FUI tone for the speculative dot: `good` when it actually ran (an active
 *  acceleration path), `idle` when normal generation ran (the neutral default) or
 *  awaiting. It is NEVER a measured-speedup verdict. PURE. */
export function speculativeTone(s: InferencePerfStatus): "good" | "idle" {
  return s.speculative === true ? "good" : "idle";
}

/** Short uppercase badge for the quant that ACTUALLY loaded (e.g. AUTO / FP16 /
 *  INT8 / INT4), or AWAITING before any turn reported. An unknown string is shown
 *  uppercased verbatim (it is what the server honestly reported). PURE. */
export function quantLabel(s: InferencePerfStatus): string {
  if (s.quant === null) return "AWAITING";
  return s.quant.toUpperCase();
}

/** True iff the reported quant is one the panel recognizes (auto/fp16/int8/int4).
 *  An unrecognized quant still renders (honest) but reads neutral. PURE. */
export function quantIsKnown(s: InferencePerfStatus): boolean {
  return s.quant !== null && KNOWN_QUANTS.has(s.quant);
}

/** Short uppercase label for the throttle reason: LOW BATTERY / THERMAL, or empty
 *  when nothing throttled. PURE. */
export function throttleReasonLabel(p: ThrottlePlan | null): string {
  if (p === null) return "";
  switch (p.reason) {
    case "low_battery":
      return "LOW BATTERY";
    case "thermal":
      return "THERMAL";
    // disabled/nominal never reach here (parseThrottle drops them), but TS wants
    // exhaustiveness — render nothing rather than a misleading word.
    case "disabled":
    case "nominal":
      return "";
  }
}

/** Short uppercase label for the throttle's preferred Local sub-tier (FAST /
 *  CAPABLE / AUTO), or empty when nothing throttled. PURE. */
export function throttleTierPrefLabel(p: ThrottlePlan | null): string {
  if (p === null) return "";
  switch (p.tierPref) {
    case "fast":
      return "FAST";
    case "capable":
      return "CAPABLE";
    case "auto":
      return "AUTO";
  }
}

/** FUI tone for the throttle dot: `warn` when a plan is throttling (a state worth
 *  noticing — the device asked JARVIS to ease off), `idle` when nothing throttled
 *  (the OFF/neutral default). PURE. */
export function throttleTone(p: ThrottlePlan | null): "warn" | "idle" {
  return p === null ? "idle" : "warn";
}

/** An HONEST one-line gloss on the speculative-decoding state for the hover copy.
 *  States the device-gated reality: ON means it ACTUALLY ran (a real but device/
 *  model-dependent speedup, never measured here); OFF/AWAITING is honest normal
 *  generation. NEVER claims a perf number. PURE. */
export function speculativeHonest(s: InferencePerfStatus): string {
  if (s.speculative === null) {
    return "Awaiting the first answered turn — no speculative path reported yet.";
  }
  if (s.speculative) {
    return (
      "Speculative decoding ACTUALLY ran this turn: a small draft model proposed " +
      "tokens the main model verified in bulk. The real speedup is device/model-" +
      "dependent and is NOT measured here — this reports only that the path ran."
    );
  }
  return (
    "Normal generation ran this turn (no speculative path). Speculative decoding " +
    "needs a configured + loadable draft model; a missing/unloadable draft honestly " +
    "falls back to normal gen and reports OFF — it never fakes speculative."
  );
}

/** An HONEST one-line gloss on the loaded quant for the hover copy. States it is
 *  the quant that ACTUALLY loaded (with the honest fallback note), and that the
 *  RAM/speed/quality tradeoff is device-gated and not measured here. PURE. */
export function quantHonest(s: InferencePerfStatus): string {
  if (s.quant === null) {
    return "Awaiting the first answered turn — no loaded quant reported yet.";
  }
  if (s.quant === "auto") {
    return (
      "Loaded the model as configured (auto — today's behavior). The RAM/speed/" +
      "quality tradeoff of an explicit quant is device-gated and not measured here."
    );
  }
  return (
    `The model loaded at ${s.quant.toUpperCase()} — the quant that ACTUALLY loaded, ` +
    "not merely the one requested (if the requested variant wasn't present the " +
    "server fell back to the available one and reports THAT). The RAM/speed/quality " +
    "tradeoff is device-gated and not measured here."
  );
}

/** An HONEST one-line gloss on the throttle plan for the hover copy. States the
 *  reason + what it prefers, and that the LIVE power read is device-gated (only
 *  when [power].adaptive is on) — never a measured thermal/battery number. PURE. */
export function throttleHonest(p: ThrottlePlan | null): string {
  if (p === null) {
    return (
      "No throttle: the plan is neutral (the OFF default — [power].adaptive off, so " +
      "nothing reads power and the tier is unchanged). The live power read (pmset / " +
      "thermal pressure / IOKit) is device-gated and happens ONLY when adaptive is on."
    );
  }
  const why =
    p.reason === "low_battery"
      ? "low battery (and not on AC)"
      : "serious/critical thermal pressure";
  const defer = p.deferHeavy ? " and defers heavy work this turn" : "";
  return (
    `Throttling because of ${why}: the plan prefers the ${throttleTierPrefLabel(p)} ` +
    `local sub-tier${defer}. The live power read is device-gated (only when ` +
    "[power].adaptive is on); this reports the plan, never a measured perf number."
  );
}

/* ------------------------------------------------------------------------ *
 * OFFLINE TOOL-LOOP — daemon/src/router.rs + anthropic.rs (task #3, item 2).    *
 * When the active tier is Local (cloud unreachable / "work offline") the daemon *
 * runs a BOUNDED on-device tool-loop: it prompts the resident ~4B with a        *
 * CURATED SAFE LOCAL-tool subset (memory recall, doc-search, skills, confined   *
 * file-read — read/compute only, NO outward/cloud tools), parses its tool-call  *
 * output, EXECUTES each call through the SAME gated execute_tool path as online *
 * (so the consequential confirmation gate, the owner voice-id gate, lockdown,   *
 * and the per-agent policies ALL still apply), feeds the result back for up to N *
 * rounds, then falls back to a plain converse answer. It surfaces THREE          *
 * secret-free `local` events the HUD folds into the ACTING-OFFLINE indicator:    *
 *                                                                                *
 *   - `local_tools.engaged` {tools_used, tools, gated, intent} — emitted ONCE    *
 *     per turn when a safe local tool actually RAN offline (the ACTING OFFLINE   *
 *     signal). `gated` is true when ANY executed tool parked/refused at a safety  *
 *     gate (for the honest HUD copy that the SAME gates apply offline).           *
 *   - `local_tools.executed` {tool, agent, is_error, outcome} — per executed     *
 *     tool (outcome capped to 120 chars by the daemon). The activity trace.       *
 *   - `local_tools.out_of_subset` {tool, agent} — the 4B hallucinated a tool      *
 *     outside the safe subset and it was REFUSED (never executed). The honest     *
 *     "the small model reached outside its safe set and we stopped it" signal.    *
 *                                                                                *
 * HONESTY (carried verbatim to the HUD copy — do NOT over-state):                *
 *   - This is REAL but BOUNDED agency over SAFE LOCAL tools while offline.        *
 *   - The on-device 4B is LESS RELIABLE at tool-calling than the cloud model      *
 *     (a genuine ceiling): it is bounded (<=N rounds) and falls back gracefully.  *
 *   - Offline tool-use does NOT bypass ANY safety gate — the SAME confirmation,   *
 *     voice-id, lockdown, and policy gates apply (the daemon runs every call      *
 *     through execute_tool); `gated` surfaces when one of those gates fired.       *
 *   - No safety gate is a tier setting — the indicator is ACTIVITY-ONLY (what the *
 *     on-device path just did), it changes NOTHING.                               *
 * The model.tier telemetry already marks the Local tier this turn — this         *
 * indicator adds only the WHAT-IT-DID (tools) layer, never a tier change.         *
 * ------------------------------------------------------------------------ */

/** Channel-"local" event names the daemon emits while the offline tool-loop runs
 *  (daemon/src/router.rs + anthropic.rs). */
export const LOCAL_TOOLS_ENGAGED_EVENT = "local_tools.engaged";
export const LOCAL_TOOLS_EXECUTED_EVENT = "local_tools.executed";
export const LOCAL_TOOLS_OUT_OF_SUBSET_EVENT = "local_tools.out_of_subset";

/** One executed (or refused) tool in the offline loop, for the activity trace.
 *  `tool` is the tool name; `agent` the namespace it ran under; `isError` is the
 *  daemon's execute_tool error flag (a gate REFUSAL reads true); `outcome` is the
 *  first 120 chars of the tool result (already capped on the wire). `outOfSubset`
 *  marks a hallucinated tool the daemon refused BEFORE execution (never ran). */
export interface LocalToolExec {
  tool: string;
  agent: string | null;
  isError: boolean;
  outcome: string;
  outOfSubset: boolean;
}

/** The live OFFLINE TOOL-LOOP surface the HUD shows. Folds the per-turn
 *  `local_tools.engaged` verdict together with the per-tool `executed` /
 *  `out_of_subset` activity so the ACTING-OFFLINE indicator can render WHETHER the
 *  on-device path is using local tools this turn, WHICH tools, and whether a
 *  safety gate fired (the honest "same gates apply offline" copy).
 *
 *  `engaged` is false until the first `local_tools.engaged` is seen — the resting
 *  state is "chatting" (the on-device path answers conversationally, no tools).
 *  `toolsUsed`/`tools` describe the last engaged turn. `gated` is true when any
 *  executed tool parked/refused at a safety gate. `refusedOutOfSubset` flags the
 *  honest case where the 4B reached outside the safe subset and was stopped.
 *  `recent` is a small bounded ring of the latest executed/refused tools (the
 *  activity trace, newest last). ACTIVITY-ONLY — carries no secret, changes no
 *  gate. */
export interface LocalToolsStatus {
  /** true while the offline loop actually ran a safe local tool this turn (ACTING
   *  OFFLINE); false in the resting "chatting" state. */
  engaged: boolean;
  /** How many safe local tools ran on the last engaged turn (0 when chatting). */
  toolsUsed: number;
  /** WHICH safe local tools ran on the last engaged turn (newest order from the
   *  daemon). */
  tools: string[];
  /** true when ANY executed tool parked/refused at a safety gate this turn — the
   *  honest signal that the SAME gates apply offline. */
  gated: boolean;
  /** The intent label of the last engaged turn (context only), null before any. */
  intent: string | null;
  /** true when the 4B named a tool OUTSIDE the safe subset and the daemon REFUSED
   *  it (never executed) — the honest "the small model reached outside its safe
   *  set and we stopped it" flag. */
  refusedOutOfSubset: boolean;
  /** A bounded ring of the most recent executed/refused tools (activity trace,
   *  newest last), capped to LOCAL_TOOLS_TRACE_MAX. */
  recent: LocalToolExec[];
}

/** The bound on the in-HUD activity trace ring (NOT the daemon's loop bound — that
 *  is config.local_tools.max_rounds). Keeps the indicator's memory tiny. */
export const LOCAL_TOOLS_TRACE_MAX = 8;

/** The resting offline-tool-loop status before any local_tools.* event: NOT
 *  engaged (the on-device path is chatting, not using tools), nothing gated.
 *  Used as the reducer seed so the indicator renders the honest "chatting" resting
 *  state immediately. */
export function localToolsInitial(): LocalToolsStatus {
  return {
    engaged: false,
    toolsUsed: 0,
    tools: [],
    gated: false,
    intent: null,
    refusedOutOfSubset: false,
    recent: [],
  };
}

/** Apply a `local_tools.engaged` payload (emitted ONCE per turn when a safe local
 *  tool actually ran offline — the ACTING OFFLINE signal). Reads ONLY
 *  {tools_used, tools, gated, intent}. `tools` non-string entries are dropped;
 *  `gated` defaults to false (an unknown gate posture reads as NOT gated — never a
 *  fake "a gate fired"). Resets `refusedOutOfSubset` for the fresh turn. Never
 *  throws. */
export function applyLocalToolsEngaged(
  prev: LocalToolsStatus,
  data: Record<string, unknown>,
): LocalToolsStatus {
  const tools = strArr(data, "tools") ?? [];
  const toolsUsed = num(data, "tools_used") ?? tools.length;
  return {
    ...prev,
    engaged: true,
    toolsUsed,
    tools,
    gated: bool(data, "gated") ?? false,
    intent: str(data, "intent") ?? prev.intent,
    // A fresh engaged turn supersedes any prior out-of-subset refusal flag; the
    // per-tool out_of_subset event for THIS turn re-raises it if it recurs.
    refusedOutOfSubset: false,
  };
}

/** Apply a `local_tools.executed` payload (per executed tool). Reads ONLY
 *  {tool, agent, is_error, outcome}. Returns prev unchanged when `tool` is absent
 *  (nothing to trace). Pushes the tool onto the bounded `recent` ring (newest
 *  last, capped to LOCAL_TOOLS_TRACE_MAX). Never throws. */
export function applyLocalToolsExecuted(
  prev: LocalToolsStatus,
  data: Record<string, unknown>,
): LocalToolsStatus {
  const tool = str(data, "tool");
  if (tool === null || tool.length === 0) return prev;
  const exec: LocalToolExec = {
    tool,
    agent: str(data, "agent"),
    isError: bool(data, "is_error") ?? false,
    outcome: str(data, "outcome") ?? "",
    outOfSubset: false,
  };
  return {
    ...prev,
    recent: [...prev.recent, exec].slice(-LOCAL_TOOLS_TRACE_MAX),
  };
}

/** Apply a `local_tools.out_of_subset` payload (the 4B named a tool outside the
 *  safe subset and the daemon REFUSED it — never executed). Reads ONLY
 *  {tool, agent}. Returns prev unchanged when `tool` is absent. Raises
 *  `refusedOutOfSubset` and traces the refusal (outOfSubset: true, isError: true —
 *  it is a refusal). Never throws. */
export function applyLocalToolsOutOfSubset(
  prev: LocalToolsStatus,
  data: Record<string, unknown>,
): LocalToolsStatus {
  const tool = str(data, "tool");
  if (tool === null || tool.length === 0) return prev;
  const exec: LocalToolExec = {
    tool,
    agent: str(data, "agent"),
    isError: true,
    outcome: "refused — outside the safe offline tool subset",
    outOfSubset: true,
  };
  return {
    ...prev,
    refusedOutOfSubset: true,
    recent: [...prev.recent, exec].slice(-LOCAL_TOOLS_TRACE_MAX),
  };
}

/** Short uppercase label for the offline-agency indicator: "ACTING OFFLINE" when
 *  the on-device path is using safe local tools this turn, "CHATTING" in the
 *  resting state. PURE. */
export function localToolsLabel(s: LocalToolsStatus): "ACTING OFFLINE" | "CHATTING" {
  return s.engaged ? "ACTING OFFLINE" : "CHATTING";
}

/** FUI tone class for the offline-agency dot. ACTING OFFLINE with a gate that
 *  fired reads `warn` (amber — a safety gate parked/refused offline, worth
 *  noticing); plain ACTING OFFLINE is `ok` (cyan — the on-device path is actively
 *  using local tools); the resting "chatting" state is `idle` (dim). PURE. */
export function localToolsTone(s: LocalToolsStatus): "warn" | "ok" | "idle" {
  if (!s.engaged) return "idle";
  return s.gated || s.refusedOutOfSubset ? "warn" : "ok";
}

/** The HONEST hover/caption copy for the offline-agency indicator. States plainly
 *  that the on-device ~4B is using SAFE LOCAL tools while offline, that it is LESS
 *  RELIABLE at tool-calling than the cloud model (a real ceiling — bounded + falls
 *  back), and that the SAME safety gates (confirmation, voice-id, lockdown, policy)
 *  still apply. When a gate fired (`gated`) or a hallucinated tool was refused
 *  (`refusedOutOfSubset`) the copy says so — the proof the gates hold offline.
 *  PURE. NEVER claims the 4B's tool-calling quality is measured. */
export function localToolsHonest(s: LocalToolsStatus): string {
  if (!s.engaged) {
    return (
      "CHATTING — the on-device model is answering conversationally; no local tools " +
      "ran this turn. When offline it CAN use a curated SAFE set of local " +
      "read/compute tools (memory recall, doc-search, skills, confined file-read) " +
      "— never outward/cloud tools."
    );
  }
  const base =
    `ACTING OFFLINE — the on-device ~4B used ${s.toolsUsed} safe local ` +
    `tool${s.toolsUsed === 1 ? "" : "s"}` +
    (s.tools.length > 0 ? ` (${s.tools.join(", ")})` : "") +
    ". These are LOCAL read/compute tools only (memory recall, doc-search, skills, " +
    "confined file-read) — NO outward/cloud tools. The on-device model is LESS " +
    "RELIABLE at tool-calling than the cloud model (a real ceiling) — the loop is " +
    "BOUNDED and falls back to a plain answer. The SAME safety gates " +
    "(confirmation, voice-id, lockdown, policy) ALL still apply offline — " +
    "offline tool-use bypasses NOTHING.";
  const gated = s.gated
    ? " A safety gate FIRED this turn (a consequential tool parked/refused) — the " +
      "proof the gates hold offline exactly as online."
    : "";
  const refused = s.refusedOutOfSubset
    ? " The on-device model named a tool OUTSIDE the safe subset and it was REFUSED " +
      "(never executed) — an offline turn can never reach an outward/cloud tool."
    : "";
  return base + gated + refused;
}

/* ------------------------------------------------------------------------ *
 * SKILLS MARKETPLACE — daemon/src/skills/mod.rs::Registry::catalog_snapshot.    *
 * The daemon emits ONE `system / skills.catalog` event after startup: the       *
 * hand-written in-tree skill library the read-only HUD Skills panel browses.     *
 * SECRET-FREE by construction on the daemon side (a pure skill carries nothing   *
 * secret; the snapshot is bounded to the discovery surface — name, category,     *
 * description, and the consequential/source-gated markers), and the parser below *
 * carries that contract forward: it surfaces ONLY those fields, so even a        *
 * malformed payload cannot smuggle an extra field into the panel. HONEST counts: *
 * `count` is the REAL shipped total (never a marketing figure); `categories`     *
 * carries each heading's count even when zero. `enabled` reflects the live       *
 * [skills] master switch (ships ON — pure skills are safe to offer).             *
 * ------------------------------------------------------------------------ */

/** One skill in the catalog (skills.catalog `skills[]`). `category` is the
 *  stable snake_case slug it lists under; `description` is the one-line "when to
 *  use". `consequential` (parks behind the confirmation gate when invoked) and
 *  `sourceGated` (reports it needs a data source until one is configured) are the
 *  two safety markers the panel badges. Both default to false when absent. */
export interface SkillEntry {
  name: string;
  category: string;
  description: string;
  consequential: boolean;
  sourceGated: boolean;
}

/** One category heading + its skill count (skills.catalog `categories[]`). Kept
 *  separate from the skills array so the panel can show a heading's count even
 *  when it has zero skills. */
export interface SkillCategoryCount {
  slug: string;
  count: number;
}

/** The whole skills surface (skills.catalog). `enabled` is the `[skills].enabled`
 *  master switch (ships ON). `count` is the REAL shipped total. `categories` is
 *  every heading with its count; `skills` is every skill in catalog order. */
export interface SkillsCatalog {
  enabled: boolean;
  count: number;
  categories: SkillCategoryCount[];
  skills: SkillEntry[];
}

/** Coerce one untrusted skill object into a SkillEntry, or null if it has no
 *  usable name or category (the structural anchors — an unnamed/uncategorized
 *  skill is not addressable). `description` defaults to "" (rendered as a dim
 *  placeholder). Both markers default to false (a pure read-only skill) when
 *  absent — the panel never over-states a marker it was not told about, matching
 *  the daemon which always emits them. DELIBERATELY surfaces ONLY the secret-free
 *  discovery fields; any extra field on the wire is ignored. Never throws. */
function coerceSkill(o: Record<string, unknown>): SkillEntry | null {
  const name = str(o, "name");
  if (name === null || name.length === 0) return null;
  const category = str(o, "category");
  if (category === null || category.length === 0) return null;
  return {
    name,
    category,
    description: str(o, "description") ?? "",
    consequential: bool(o, "consequential") ?? false,
    sourceGated: bool(o, "source_gated") ?? false,
  };
}

/** Coerce one untrusted category-count object, or null if it has no usable slug.
 *  `count` defaults to 0 and is clamped to a non-negative integer. Never throws. */
function coerceSkillCategory(o: Record<string, unknown>): SkillCategoryCount | null {
  const slug = str(o, "slug");
  if (slug === null || slug.length === 0) return null;
  const raw = num(o, "count");
  const count = raw !== null && raw >= 0 ? Math.floor(raw) : 0;
  return { slug, count };
}

/** Parse a skills.catalog payload. `enabled` defaults to true (the shipped-ON
 *  posture for the pure library) when absent/non-bool. `count` defaults to the
 *  length of the parsed skills array when absent/invalid (so the panel never
 *  shows a count that disagrees with what it renders). `categories` and `skills`
 *  default to [] and are coerced item-by-item (malformed entries dropped). NEVER
 *  returns null — a catalog frame always yields a snapshot so the panel can render
 *  the honest state. NEVER carries a secret. Never throws on junk. */
export function parseSkillsCatalog(data: Record<string, unknown>): SkillsCatalog {
  const rawSkills = data["skills"];
  const skills = Array.isArray(rawSkills)
    ? rawSkills
        .filter(isPlainObject)
        .map(coerceSkill)
        .filter((s): s is SkillEntry => s !== null)
    : [];
  const rawCategories = data["categories"];
  const categories = Array.isArray(rawCategories)
    ? rawCategories
        .filter(isPlainObject)
        .map(coerceSkillCategory)
        .filter((c): c is SkillCategoryCount => c !== null)
    : [];
  const rawCount = num(data, "count");
  const count = rawCount !== null && rawCount >= 0 ? Math.floor(rawCount) : skills.length;
  return {
    enabled: bool(data, "enabled") ?? true,
    count,
    categories,
    skills,
  };
}

/* ------------------------------------------------------------------------ *
 * EVAL / OPTIMIZER scorecard — daemon/src/eval.rs::report_snapshot, emitted   *
 * as a `system / eval.report` envelope by the periodic eval_report_task        *
 * (every 30s, 20s startup delay). AGGREGATE-ONLY + no PII: the wire carries     *
 * ONLY percentiles, token SUMS, rates, counts, and the honest optimizer         *
 * posture — never an utterance, a per-turn row, or an identifier. The parser    *
 * below carries that contract forward and is the single narrowing point for the *
 * Eval/Optimizer panel.                                                         *
 *                                                                               *
 * HONESTY (do not regress): latency + cost are RUNTIME-GATED (fed by real       *
 * turns/cloud calls), so a fresh daemon emits `status:"awaiting turns"` for     *
 * them — never a fabricated number. routing_accuracy / correction_rate arrive   *
 * either as a 0..1 number OR the literal string "awaiting turns" when the       *
 * window/corpus is empty; we map that string to `null` so the panel renders an  *
 * honest placeholder rather than inventing a value. The optimizer is            *
 * PROPOSE-ONLY + OFF by default — `enabled`/`mode` describe the real config and *
 * the eval framework NEVER tunes anything.                                      *
 * ------------------------------------------------------------------------ */

/** The measured latency aggregate (eval.rs LatencyAggregate). `measured` is
 *  false when the rolling window is empty (`status:"awaiting turns"`) — the
 *  per-stage p50/p95 fields are meaningless and stay 0 in that case, so the
 *  panel must gate on `measured`, never render a fake "0 ms". All values are
 *  REAL milliseconds measured by the existing pipeline clocks. */
export interface EvalLatency {
  measured: boolean;
  n: number;
  totalP50Ms: number;
  totalP95Ms: number;
  queueP50Ms: number;
  queueP95Ms: number;
  sttP50Ms: number;
  sttP95Ms: number;
  classifyP50Ms: number;
  classifyP95Ms: number;
  routeP50Ms: number;
  routeP95Ms: number;
}

/** The rolling cost aggregate (eval.rs CostAggregate). `measured` is false until
 *  the cloud reply path has surfaced token `usage` for at least one turn
 *  (`status:"awaiting turns"`). Tokens are the TRUTH; `estCostUsd` is a
 *  transparently-LABELLED estimate (a published $/1M multiplier, never a billed
 *  figure) — `costIsEstimate` is always true on the wire and mirrored here so the
 *  panel's "EST." label is grounded in the payload. */
export interface EvalCost {
  measured: boolean;
  n: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  totalTokens: number;
  estCostUsd: number;
  costIsEstimate: boolean;
}

/** The accuracy aggregate (eval.rs AccuracyAggregate). `routingAccuracy` /
 *  `correctionRate` are 0..1 OR null when the daemon reported "awaiting turns"
 *  (empty held-out split / no usable traces). `heldOutN` / `usableN` are the
 *  honest denominators so the panel can show "n=" beside each rate; `corrections`
 *  is the raw count behind the correction rate. Both rates are MEASURED from real
 *  recorded traces — never fabricated. */
export interface EvalAccuracy {
  routingAccuracy: number | null;
  heldOutN: number;
  correctionRate: number | null;
  corrections: number;
  usableN: number;
}

/** The optimizer posture echoed in the eval.report (eval.rs `optimizer`). This
 *  describes the PROPOSE-ONLY + OFF-by-default optimizer so the panel's copy is
 *  grounded in the real config — `enabled` is the `[optimize].enabled` master
 *  switch (ships false), `mode` the configured mode, and `posture` is always
 *  "propose-only" (the eval framework NEVER tunes anything). */
export interface EvalOptimizer {
  enabled: boolean;
  mode: string;
  posture: string;
}

/** The whole AGGREGATE-ONLY eval scorecard the Eval/Optimizer panel renders.
 *  `runtimeGated` lists the aggregates whose LIVE feed needs real turns/cloud
 *  calls (today: latency + cost) — the panel uses it for honest copy. NEVER
 *  carries PII. */
export interface EvalReport {
  latency: EvalLatency;
  cost: EvalCost;
  accuracy: EvalAccuracy;
  optimizer: EvalOptimizer;
  runtimeGated: string[];
}

/** A non-negative integer or 0 (counts/token sums on the wire are u64). Floors a
 *  fractional value and clamps a negative/absent one to 0 so a garbled count can
 *  never render as a negative or NaN. */
function nonNegInt(data: Record<string, unknown>, key: string): number {
  const v = num(data, key);
  return v !== null && v >= 0 ? Math.floor(v) : 0;
}

/** A 0..1 rate, or null. The daemon sends EITHER a finite number OR the literal
 *  string "awaiting turns" (metric_or_awaiting in eval.rs) — anything else (the
 *  string, a non-finite, an out-of-range number) maps to null so the panel shows
 *  an honest "awaiting turns" placeholder rather than a fabricated rate. */
function rateOrNull(data: Record<string, unknown>, key: string): number | null {
  const v = num(data, key);
  if (v === null) return null; // includes the "awaiting turns" string case
  return v >= 0 && v <= 1 ? v : null;
}

/** Parse the latency sub-object. `measured` is gated on BOTH a non-"awaiting
 *  turns" status AND a positive `n`, so a malformed/empty frame is never read as
 *  a measurement. */
function parseEvalLatency(data: Record<string, unknown>): EvalLatency {
  const n = nonNegInt(data, "n");
  const measured = str(data, "status") === "measured" && n > 0;
  return {
    measured,
    n,
    totalP50Ms: nonNegInt(data, "total_p50_ms"),
    totalP95Ms: nonNegInt(data, "total_p95_ms"),
    queueP50Ms: nonNegInt(data, "queue_p50_ms"),
    queueP95Ms: nonNegInt(data, "queue_p95_ms"),
    sttP50Ms: nonNegInt(data, "stt_p50_ms"),
    sttP95Ms: nonNegInt(data, "stt_p95_ms"),
    classifyP50Ms: nonNegInt(data, "classify_p50_ms"),
    classifyP95Ms: nonNegInt(data, "classify_p95_ms"),
    routeP50Ms: nonNegInt(data, "route_p50_ms"),
    routeP95Ms: nonNegInt(data, "route_p95_ms"),
  };
}

/** Parse the cost sub-object. `measured` is gated on status + a positive `n`.
 *  `estCostUsd` stays a non-negative float (it is a dollar estimate, not an int);
 *  `costIsEstimate` defaults TRUE (fail-safe: a cost figure is always shown as an
 *  estimate, never as a billed number). */
function parseEvalCost(data: Record<string, unknown>): EvalCost {
  const n = nonNegInt(data, "n");
  const measured = str(data, "status") === "measured" && n > 0;
  const rawCost = num(data, "est_cost_usd");
  return {
    measured,
    n,
    inputTokens: nonNegInt(data, "input_tokens"),
    outputTokens: nonNegInt(data, "output_tokens"),
    cacheReadTokens: nonNegInt(data, "cache_read_tokens"),
    totalTokens: nonNegInt(data, "total_tokens"),
    estCostUsd: rawCost !== null && rawCost >= 0 ? rawCost : 0,
    costIsEstimate: bool(data, "cost_is_estimate") ?? true,
  };
}

/** Parse the accuracy sub-object. Both rates map the "awaiting turns" string to
 *  null; the denominators/counts default to 0. */
function parseEvalAccuracy(data: Record<string, unknown>): EvalAccuracy {
  return {
    routingAccuracy: rateOrNull(data, "routing_accuracy"),
    heldOutN: nonNegInt(data, "held_out_n"),
    correctionRate: rateOrNull(data, "correction_rate"),
    corrections: nonNegInt(data, "corrections"),
    usableN: nonNegInt(data, "usable_n"),
  };
}

/** Parse an `eval.report` payload. NEVER returns null — like mcp.status, an eval
 *  report always yields a (possibly all-"awaiting turns") snapshot so the panel
 *  can render the honest empty state rather than a stale one. Every sub-object is
 *  narrowed defensively (a missing/garbled sub-object collapses to an empty
 *  one). AGGREGATE-ONLY: no field here can carry PII. Never throws on junk. */
export function parseEvalReport(data: Record<string, unknown>): EvalReport {
  const latencyObj = isPlainObject(data["latency"]) ? data["latency"] : {};
  const costObj = isPlainObject(data["cost"]) ? data["cost"] : {};
  const accuracyObj = isPlainObject(data["accuracy"]) ? data["accuracy"] : {};
  const optimizerObj = isPlainObject(data["optimizer"]) ? data["optimizer"] : {};
  return {
    latency: parseEvalLatency(latencyObj),
    cost: parseEvalCost(costObj),
    accuracy: parseEvalAccuracy(accuracyObj),
    optimizer: {
      enabled: bool(optimizerObj, "enabled") ?? false,
      mode: str(optimizerObj, "mode") ?? "",
      // Honest fixed posture — the eval framework only measures, never tunes.
      posture: str(optimizerObj, "posture") ?? "propose-only",
    },
    runtimeGated: strArr(data, "runtime_gated") ?? [],
  };
}

/** A pending optimizer PROPOSAL, folded from a `system / optimize.proposed`
 *  event (optimize.rs run_optimizer). The optimizer is PROPOSE-ONLY: this is a
 *  REVIEWABLE artifact written under state/optimize/proposals/<ts>/, NOT a live
 *  config change. The panel surfaces it READ-ONLY and points the user at the
 *  MANUAL apply step (scripts/apply_optimization.sh) — there is deliberately NO
 *  one-click apply. SECRET-FREE: only the staging ts, the measured held-out
 *  improvement, and the change count cross the wire (never a routing rule or an
 *  utterance). */
export interface OptimizerProposal {
  /** Staging unix timestamp — the <ts> for scripts/apply_optimization.sh. */
  ts: number;
  /** Measured held-out accuracy improvement (candidate − baseline), 0..1. */
  improvement: number;
  /** Baseline / candidate held-out routing accuracy (0..1), for transparency. */
  baselineAccuracy: number | null;
  candidateAccuracy: number | null;
  /** How many routing-config entries the proposal would change. */
  changes: number;
}

/** Parse an `optimize.proposed` payload into an OptimizerProposal, or null
 *  unless BOTH a finite `ts` AND a finite `improvement` are present — the panel
 *  must never show a proposal it cannot point an apply command at, nor one with
 *  no measured win. `baseline_accuracy`/`candidate_accuracy` stay null when
 *  absent. Never throws. SECRET-FREE. */
export function parseOptimizerProposal(
  data: Record<string, unknown>,
): OptimizerProposal | null {
  const ts = num(data, "ts");
  const improvement = num(data, "improvement");
  if (ts === null || improvement === null) return null;
  return {
    ts,
    improvement,
    baselineAccuracy: num(data, "baseline_accuracy"),
    candidateAccuracy: num(data, "candidate_accuracy"),
    changes: nonNegInt(data, "changes"),
  };
}

/* ------------------------------------------------------------------------ *
 * DOC SEARCH — on-device file RAG (daemon/src/docsearch.rs).                      *
 *                                                                                *
 * Two SECRET-FREE local events feed the read-only DocSearchPanel:                *
 *   - `local / docsearch.indexed` ({files, chunks, embedded_chunks}) — emitted   *
 *     by router.rs::handle_docsearch_index after a confined reindex over the     *
 *     EXPLICITLY-ALLOWLISTED [docsearch].roots. It carries only COUNTS — no file *
 *     path, no chunk text, no vector. `embedded_chunks` vs `chunks` tells the    *
 *     HUD whether search will run NEURAL (all chunks embedded on-device) or fall *
 *     back to lexical BM25 (the on-device embedder was down at index time).      *
 *   - `local / docsearch.searched` ({query, method, hits[]}) — emitted by        *
 *     anthropic.rs::doc_search_tool. `hits` CITE real indexed chunks (the user's *
 *     OWN allowlisted file paths + bounded snippets they are explicitly          *
 *     searching — the same text the persona speaks aloud / shows in the          *
 *     transcript). `method` is the backend that ACTUALLY ran, so the panel never *
 *     claims neural when it fell back to BM25. The daemon NEVER fabricates a hit;  *
 *     the parser carries that forward — it surfaces ONLY real returned hits.     *
 *                                                                                *
 * 100% ON-DEVICE: telemetry is the local 127.0.0.1 broadcast only — file         *
 * contents + embeddings never leave the device. SHIPPED-OFF: the feature is      *
 * disabled by default and indexes nothing until the operator flips [docsearch].  *
 * enabled AND allowlists a root, so these events simply never arrive until then. *
 * ------------------------------------------------------------------------ */

/** Which ranking backend the daemon's recall layer ACTUALLY ran (recall.rs
 *  RankMethod::as_str). The panel reports this honestly — "neural-embedding" is
 *  true on-device cosine over embedding vectors; "lexical-bm25" is the keyword
 *  fallback used when the on-device embedder was unavailable. Tolerant of any
 *  other string (rendered verbatim) so a future method never breaks the panel. */
export type DocSearchMethod = "neural-embedding" | "lexical-bm25" | string;

/** The on-device file index status (docsearch.indexed / DocIndex::status). COUNTS
 *  ONLY — never a path or chunk text. `embeddedChunks` < `chunks` means some
 *  chunks have no on-device vector (the embedder was down at index time), so a
 *  search over them falls back to BM25; `embeddedChunks === chunks > 0` means a
 *  search runs fully neural. */
export interface DocIndexStatus {
  files: number;
  chunks: number;
  embeddedChunks: number;
}

/** One CITED search hit (docsearch.searched `hits[]` / docsearch.rs DocHit). The
 *  citation anchor is `filePath` + `byteOffset`; `snippet` is the bounded
 *  (<=280-char, boundary-safe) chunk text the daemon already cited. Only ever
 *  built from a REAL returned hit — never fabricated. `root` is the allowlisted
 *  folder it came from. */
export interface DocHit {
  filePath: string;
  root: string;
  byteOffset: number;
  snippet: string;
  score: number;
}

/** A complete cited search result (docsearch.searched / docsearch.rs
 *  DocSearchResult): the query, the cited hits, and the method that ACTUALLY
 *  ran. An empty `hits` is an HONEST "nothing found" (no index built / no match)
 *  — never a fabricated citation. */
export interface DocSearchResult {
  query: string;
  hits: DocHit[];
  method: DocSearchMethod;
}

/** Parse a `docsearch.indexed` payload into a DocIndexStatus. All three counts
 *  default to 0 (the honest empty-index state) when absent/non-numeric, and
 *  `embeddedChunks` is clamped to <= `chunks` so the panel can never claim more
 *  chunks are embedded than exist. NEVER returns null — an index event always
 *  yields an honest status. Never throws. */
export function parseDocIndexStatus(data: Record<string, unknown>): DocIndexStatus {
  const files = nonNegInt(data, "files");
  const chunks = nonNegInt(data, "chunks");
  const embeddedChunks = Math.min(nonNegInt(data, "embedded_chunks"), chunks);
  return { files, chunks, embeddedChunks };
}

/** Coerce one untrusted hit object into a DocHit, or null if it has no usable
 *  `file_path` (the citation anchor — a hit with no file to point at is not a
 *  real citation, so it is dropped). Every other field defaults safely. Never
 *  throws. */
function coerceDocHit(o: Record<string, unknown>): DocHit | null {
  const filePath = str(o, "file_path");
  if (filePath === null || filePath.length === 0) return null;
  return {
    filePath,
    root: str(o, "root") ?? "",
    byteOffset: nonNegInt(o, "byte_offset"),
    snippet: str(o, "snippet") ?? "",
    score: num(o, "score") ?? 0,
  };
}

/** Parse a `docsearch.searched` payload into a DocSearchResult. `hits` are
 *  coerced item-by-item (malformed entries dropped — a hit with no file_path is
 *  not a citation). `method` defaults to "lexical-bm25" (the conservative
 *  fallback) when absent, so the panel never OVER-states the result as neural.
 *  NEVER returns null — a search event always yields an honest result (an empty
 *  `hits` is the honest "nothing found"). Never throws. */
export function parseDocSearchResult(data: Record<string, unknown>): DocSearchResult {
  const rawHits = data["hits"];
  const hits = Array.isArray(rawHits)
    ? rawHits
        .filter(isPlainObject)
        .map(coerceDocHit)
        .filter((h): h is DocHit => h !== null)
    : [];
  return {
    query: str(data, "query") ?? "",
    hits,
    method: str(data, "method") ?? "lexical-bm25",
  };
}

/* ------------------------------------------------------------------------ *
 * UNIFIED SEARCH — one query fanned out across every AVAILABLE source             *
 * (daemon/src/unified_search.rs + anthropic.rs::unified_search_tool).             *
 *                                                                                *
 * ONE secret-free local event feeds the read-only UnifiedSearchPanel:            *
 *   - `local / unified.searched` ({query, coverage{searched[], skipped[]},       *
 *     hits[]}) — emitted by anthropic.rs::unified_search_tool after the pure      *
 *     merge/rank/coverage core (unified_search::fold) ran. It carries ONLY what  *
 *     the persona already speaks: the query, the HONEST coverage (which sources   *
 *     were SEARCHED vs SKIPPED, each skip with a machine-stable reason), and the  *
 *     real merged hits — each ATTRIBUTED to its source and carrying a real        *
 *     CITATION anchor: a per-item anchor for on-device sources (file path+offset  *
 *     / episode ts / fact key / world entity), and an honest SOURCE-LEVEL anchor  *
 *     for a cloud-summary hit (the gated read it came from, e.g. "gmail recent    *
 *     messages (search: …)") — NOT a fabricated message/event id. The daemon      *
 *     NEVER fabricates a hit or a citation; the parser carries that forward — it  *
 *     surfaces ONLY real returned hits and reports coverage truthfully (a skipped *
 *     source is NEVER rendered as if it had been searched).                       *
 *                                                                                *
 * HONESTY (load-bearing, do not regress):                                        *
 *   - ON-DEVICE sources (Files/Past conversations/Memory/World model) are always *
 *     available; their content never leaves the device (this event is the local  *
 *     127.0.0.1 broadcast only). CLOUD sources (Gmail/Calendar/Slack) are        *
 *     searched ONLY when CONNECTED — a disconnected cloud source arrives as a     *
 *     SKIP with reason "not_connected", never as a silent drop or a fake hit.    *
 *   - An all-empty fan-out still carries the coverage (searched vs skipped) so    *
 *     the panel can say "searched X, Y; found nothing" rather than inventing one. *
 * ------------------------------------------------------------------------ */

/** A short, stable source token (unified_search::Source::as_str). The four
 *  on-device sources are always available; the three cloud sources are gated by a
 *  connected-check. Tolerant of any other string (rendered with a derived label)
 *  so a future source never breaks the panel. */
export type UnifiedSource =
  | "docsearch"
  | "episodic"
  | "facts"
  | "world"
  | "gmail"
  | "calendar"
  | "slack"
  | string;

/** A machine-stable skip reason (unified_search::SkipReason::as_str). Each maps
 *  to one honest human clause; an unknown future reason renders verbatim. */
export type UnifiedSkipReason = "not_connected" | "no_index" | "not_requested" | string;

/** The honest human label for a unified source (mirrors Source::label() in the
 *  daemon). The daemon also sends `source_label` per-hit; this is the fallback
 *  used for the coverage line + an unknown source. */
export function unifiedSourceLabel(source: UnifiedSource): string {
  switch (source) {
    case "docsearch":
      return "Files";
    case "episodic":
      return "Past conversations";
    case "facts":
      return "Memory";
    case "world":
      return "World model";
    case "gmail":
      return "Gmail";
    case "calendar":
      return "Calendar";
    case "slack":
      return "Slack";
    default:
      // An unknown future source — render its token verbatim rather than hide it.
      return source;
  }
}

/** Whether a unified source is ON-DEVICE (content never leaves the device). The
 *  three cloud sources are the only ones gated by a connected-check; everything
 *  else (including an unknown future source) is treated as on-device only when it
 *  is one of the four known on-device tokens. */
export function unifiedSourceOnDevice(source: UnifiedSource): boolean {
  return (
    source === "docsearch" ||
    source === "episodic" ||
    source === "facts" ||
    source === "world"
  );
}

/** The honest human clause for a skip reason (mirrors SkipReason::human()). */
export function unifiedSkipReasonLabel(reason: UnifiedSkipReason): string {
  switch (reason) {
    case "not_connected":
      return "not connected";
    case "no_index":
      return "no index built";
    case "not_requested":
      return "not requested";
    default:
      return reason;
  }
}

/** One CITED, ATTRIBUTED hit in a unified result (unified_search::UnifiedHit).
 *  `source` is the machine token; `sourceLabel` is the daemon-sent human label
 *  for the group header; `citation` is the real anchor string the daemon built
 *  (e.g. "/path (offset N)", "episode @ <ts>" for per-item anchors, or an honest
 *  source-level "gmail recent messages (search: …)" for a cloud-summary hit) —
 *  NEVER fabricated;
 *  `score` is the final blended rank (higher = more relevant); `ts` is the
 *  RFC3339 timestamp when the source carries one, else null. Only ever built from
 *  a REAL returned hit (a hit with no citation anchor is dropped). */
export interface UnifiedHit {
  source: UnifiedSource;
  sourceLabel: string;
  citation: string;
  title: string;
  snippet: string;
  score: number;
  ts: string | null;
}

/** One skipped source + the honest reason (unified_search::Skipped). NEVER used
 *  to pretend a source was searched. */
export interface UnifiedSkip {
  source: UnifiedSource;
  reason: UnifiedSkipReason;
}

/** The HONEST coverage summary of a unified search (unified_search::Coverage):
 *  exactly which sources were SEARCHED and which were SKIPPED (each with a
 *  reason). The panel renders this so the user always knows the answer's reach —
 *  never conflating searched with skipped. */
export interface UnifiedCoverage {
  searched: UnifiedSource[];
  skipped: UnifiedSkip[];
}

/** A complete unified-search result (unified.searched / UnifiedResult): the
 *  query, the honest coverage, and the merged + ranked + attributed + cited hits.
 *  An empty `hits` with a non-empty `coverage.searched` is the HONEST "I searched
 *  these sources and found nothing" result — never a fabricated hit. */
export interface UnifiedSearchResult {
  query: string;
  coverage: UnifiedCoverage;
  hits: UnifiedHit[];
}

/** Coerce one untrusted hit object into a UnifiedHit, or null if it has no usable
 *  `source` or no `citation` anchor (a hit with no source to attribute it to or
 *  no real item to cite is NOT a real citation, so it is dropped — never
 *  fabricated). Every other field defaults safely. `source_label` falls back to
 *  the derived label so the group header is always honest. Never throws. */
function coerceUnifiedHit(o: Record<string, unknown>): UnifiedHit | null {
  const source = str(o, "source");
  if (source === null || source.length === 0) return null;
  const citation = str(o, "citation");
  if (citation === null || citation.length === 0) return null;
  return {
    source,
    sourceLabel: str(o, "source_label") ?? unifiedSourceLabel(source),
    citation,
    title: str(o, "title") ?? "",
    snippet: str(o, "snippet") ?? "",
    score: num(o, "score") ?? 0,
    ts: str(o, "ts"), // null when the source carries no timestamp — honest
  };
}

/** Coerce one untrusted skip entry into a UnifiedSkip, or null if it has no
 *  usable `source` (a skip with no source to name is meaningless, so it is
 *  dropped rather than rendered as a blank). `reason` defaults to "not_requested"
 *  (the most conservative honest reason) when absent. Never throws. */
function coerceUnifiedSkip(o: Record<string, unknown>): UnifiedSkip | null {
  const source = str(o, "source");
  if (source === null || source.length === 0) return null;
  return {
    source,
    reason: str(o, "reason") ?? "not_requested",
  };
}

/** Parse the `coverage` sub-object into a UnifiedCoverage. `searched` keeps only
 *  non-empty string tokens (in the order the daemon sent them — the deterministic
 *  on-device-then-cloud order). `skipped` is coerced entry-by-entry (a malformed
 *  entry is dropped, never rendered as a fake skip). Both default to empty, so an
 *  absent/garbled coverage reads as the honest "Searched no sources." rather than
 *  throwing. Never throws. */
function parseUnifiedCoverage(data: Record<string, unknown>): UnifiedCoverage {
  const cov = isPlainObject(data["coverage"]) ? data["coverage"] : {};
  const rawSearched = (cov as Record<string, unknown>)["searched"];
  const searched = Array.isArray(rawSearched)
    ? rawSearched.filter((x): x is string => typeof x === "string" && x.length > 0)
    : [];
  const rawSkipped = (cov as Record<string, unknown>)["skipped"];
  const skipped = Array.isArray(rawSkipped)
    ? rawSkipped
        .filter(isPlainObject)
        .map(coerceUnifiedSkip)
        .filter((x): x is UnifiedSkip => x !== null)
    : [];
  return { searched, skipped };
}

/** Parse a `unified.searched` payload into a UnifiedSearchResult. `hits` are
 *  coerced item-by-item (a hit with no source or no citation anchor is dropped —
 *  it is not a real citation). `coverage` is parsed defensively (searched vs
 *  skipped, never conflated). NEVER returns null — a unified-search event always
 *  yields an honest result (an empty `hits` with a non-empty `coverage.searched`
 *  is the honest "searched X, found nothing"). Never throws. */
export function parseUnifiedSearchResult(
  data: Record<string, unknown>,
): UnifiedSearchResult {
  const rawHits = data["hits"];
  const hits = Array.isArray(rawHits)
    ? rawHits
        .filter(isPlainObject)
        .map(coerceUnifiedHit)
        .filter((h): h is UnifiedHit => h !== null)
    : [];
  return {
    query: str(data, "query") ?? "",
    coverage: parseUnifiedCoverage(data),
    hits,
  };
}

/** Build the one honest coverage sentence for the spoken/HUD line (mirrors
 *  Coverage::summary() in the daemon, computed client-side so the panel never has
 *  to trust a daemon-sent prose string). Reports the searched sources and,
 *  separately, each skipped source with its reason — NEVER conflating the two; an
 *  empty searched set says "Searched no sources." */
export function unifiedCoverageSummary(coverage: UnifiedCoverage): string {
  const searched = coverage.searched.map((s) => unifiedSourceLabel(s));
  const searchedPart =
    searched.length === 0 ? "Searched no sources" : `Searched ${searched.join(", ")}`;
  if (coverage.skipped.length === 0) return `${searchedPart}.`;
  const skipped = coverage.skipped.map(
    (s) => `${unifiedSourceLabel(s.source)} (${unifiedSkipReasonLabel(s.reason)})`,
  );
  return `${searchedPart}. Skipped ${skipped.join(", ")}.`;
}

/* ------------------------------------------------------------------------ *
 * KNOWLEDGE GRAPH — entities/relationships EXTRACTED from the user's own      *
 * indexed documents and upserted into the SHARED world model                  *
 * (daemon/src/knowledge_graph.rs + world_model.rs).                           *
 *                                                                            *
 * ONE secret-free local event feeds the read-only KnowledgeGraphPanel:        *
 *   - `local / knowledge_graph.built` — emitted by router.rs after the gated   *
 *     build pass ran the (deterministic-heuristic) extractor over the docsearch *
 *     chunks and upserted the grounded nodes/edges into `user.world.*`. It      *
 *     carries the BUILD STATS (chunks_scanned / entities_written /             *
 *     relationships_written / skipped_at_cap), the `extractor` METHOD TOKEN     *
 *     that actually ran (e.g. "deterministic-heuristic"), and a bounded `graph`  *
 *     snapshot of the resulting SHARED world model: entities (type/id/name +    *
 *     their `source` provenance string) grouped by type, and relationships      *
 *     (from/relation/to + the `source file:offset` detail on the co-occurrence  *
 *     edge).                                                                    *
 *                                                                            *
 * HONESTY (load-bearing, do not regress):                                     *
 *   - EXTRACTED, NEVER INVENTED. Every node/edge is GROUNDED in real document   *
 *     text the daemon indexed; each carries a `source` PROVENANCE citation      *
 *     (file:offset(+char span)) so the user can trace it back. An entity with   *
 *     no source is shown honestly as "no citation", never hidden or faked.      *
 *   - CONSERVATIVE HEURISTIC. The shipped extractor is a deterministic,         *
 *     on-device heuristic — it errs toward MISSING rather than inventing and is *
 *     NOT a sophisticated NER. The optional richer extractor is runtime-gated.  *
 *     The panel says so plainly — it never implies completeness.               *
 *   - SHARED + BOUNDED. Writes only the shared `user.world.*` tier (never an    *
 *     agent's private namespace) and respects the world model's entity/relation *
 *     caps — `skipped_at_cap` is surfaced as the honest "refused past the bound"*
 *     proof, never a silent unbounded grow. This event rides the local         *
 *     127.0.0.1 broadcast only.                                                 *
 *   - SHIPS OFF + REVIEW-ONLY. Double-gated ([docsearch].enabled AND           *
 *     [docsearch].build_graph, both ship false); the event never arrives until  *
 *     deliberately enabled. There is NO button here that builds or writes —     *
 *     building is a SPOKEN intent ("map my documents"); this panel only SHOWS   *
 *     the last build's stats + the resulting grouped graph.                     *
 * ------------------------------------------------------------------------ */

/** The bounded set of entity KINDS the world model recognizes
 *  (world_model.rs EntityType::as_str). Tolerant of any other string (rendered
 *  under an "OTHER" group) so a future kind never breaks the panel. */
export type KgEntityType =
  | "project"
  | "person"
  | "deadline"
  | "task"
  | "topic"
  | "thread"
  | string;

/** The canonical kind order the panel groups by (matches world_model.rs
 *  EntityType::all()). */
export const KG_ENTITY_TYPES: readonly KgEntityType[] = [
  "project",
  "person",
  "deadline",
  "task",
  "topic",
  "thread",
];

/** One extracted entity in the `knowledge_graph.built` `graph.entities[]`. `type`
 *  is the bounded kind token; `id` is the stable slug; `name` the display name;
 *  `source` is the PROVENANCE citation (`file:offset (chars a-b)`) — null when the
 *  node carries none (shown honestly as "no citation", never faked). */
export interface KgEntity {
  type: KgEntityType;
  id: string;
  name: string;
  source: string | null;
}

/** One extracted relationship in `graph.relationships[]`: the from/relation/to
 *  endpoint ids + the `source` detail recorded on the edge (for the "mentions"
 *  co-occurrence edge this is the `source file:offset` that grounds it). */
export interface KgRelationship {
  from: string;
  relation: string;
  to: string;
  source: string;
}

/** A complete `knowledge_graph.built` payload: the build STATS, the honest
 *  extractor METHOD token, and the resulting bounded SHARED-world-model snapshot
 *  (entities + relationships, each provenance-tagged). NEVER carries chunk text —
 *  counts/ids/names/source strings only. */
export interface KnowledgeGraphResult {
  chunksScanned: number;
  entitiesWritten: number;
  relationshipsWritten: number;
  skippedAtCap: number;
  extractor: string;
  entities: KgEntity[];
  relationships: KgRelationship[];
}

/** Coerce one untrusted entity object into a KgEntity, or null if it lacks a
 *  usable `id` (the stable anchor — a node with no id cannot be cited or grouped,
 *  so it is dropped). `name` defaults to the id; `source` stays null when absent
 *  (shown honestly as "no citation"). Never throws. */
function coerceKgEntity(o: Record<string, unknown>): KgEntity | null {
  const id = str(o, "id");
  if (id === null || id.length === 0) return null;
  return {
    type: str(o, "type") ?? "topic",
    id,
    name: str(o, "name") ?? id,
    source: str(o, "source"),
  };
}

/** Coerce one untrusted relationship object into a KgRelationship, or null if it
 *  lacks usable `from`/`to` endpoints (an edge with no endpoints is not a
 *  relationship). `relation` defaults to "mentions" (the co-occurrence edge);
 *  `source` defaults to "" (shown as "no citation"). Never throws. */
function coerceKgRelationship(o: Record<string, unknown>): KgRelationship | null {
  const from = str(o, "from");
  const to = str(o, "to");
  if (from === null || from.length === 0 || to === null || to.length === 0) {
    return null;
  }
  return {
    from,
    relation: str(o, "relation") ?? "mentions",
    to,
    source: str(o, "source") ?? "",
  };
}

/** Parse a `knowledge_graph.built` payload into a KnowledgeGraphResult. Stats
 *  default to 0 (the honest empty-build state); `extractor` defaults to
 *  "deterministic-heuristic" (the conservative shipped fallback) so the panel
 *  never OVER-states the method. The `graph` snapshot's entities/relationships are
 *  coerced item-by-item (malformed entries dropped — a node with no id, an edge
 *  with no endpoints, is not real). NEVER returns null — a build event always
 *  yields an honest result (an empty graph is the honest "extracted nothing").
 *  Never throws. */
export function parseKnowledgeGraphResult(
  data: Record<string, unknown>,
): KnowledgeGraphResult {
  const graph = isPlainObject(data["graph"]) ? data["graph"] : {};
  const rawEntities = graph["entities"];
  const entities = Array.isArray(rawEntities)
    ? rawEntities
        .filter(isPlainObject)
        .map(coerceKgEntity)
        .filter((e): e is KgEntity => e !== null)
    : [];
  const rawRels = graph["relationships"];
  const relationships = Array.isArray(rawRels)
    ? rawRels
        .filter(isPlainObject)
        .map(coerceKgRelationship)
        .filter((r): r is KgRelationship => r !== null)
    : [];
  return {
    chunksScanned: nonNegInt(data, "chunks_scanned"),
    entitiesWritten: nonNegInt(data, "entities_written"),
    relationshipsWritten: nonNegInt(data, "relationships_written"),
    skippedAtCap: nonNegInt(data, "skipped_at_cap"),
    extractor: str(data, "extractor") ?? "deterministic-heuristic",
    entities,
    relationships,
  };
}

/** A short, human label for an entity kind (the panel group header). Falls back
 *  to an upper-cased token for an unknown future kind so it still renders. */
export function kgEntityTypeLabel(type: KgEntityType): string {
  switch (type) {
    case "project":
      return "Projects";
    case "person":
      return "People";
    case "deadline":
      return "Deadlines";
    case "task":
      return "Tasks";
    case "topic":
      return "Topics";
    case "thread":
      return "Threads";
    default:
      return type.length > 0 ? type.toUpperCase() : "OTHER";
  }
}

/* ------------------------------------------------------------------------ *
 * Envelope parsing                                                          *
 * ------------------------------------------------------------------------ */

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/**
 * Parse one WebSocket text frame into a TelemetryEnvelope.
 *
 * Returns null for anything that is not a structurally valid envelope:
 * malformed JSON, non-object payloads, or missing/wrongly-typed `ts`,
 * `source`, or `event`. A missing or non-object `data` is coerced to `{}`
 * (be liberal — the reducer ignores fields it does not know).
 */
export function parseEnvelope(raw: string): TelemetryEnvelope | null {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!isPlainObject(value)) return null;
  const { ts, source, event, data } = value as Record<string, unknown>;
  if (typeof ts !== "string") return null;
  if (typeof source !== "string" || source.length === 0) return null;
  if (typeof event !== "string" || event.length === 0) return null;
  return {
    ts,
    source,
    event,
    data: isPlainObject(data) ? data : {},
  };
}

/* Narrowing helpers used by the reducer — tolerant of missing fields. */
export function num(data: Record<string, unknown>, key: string): number | null {
  const v = data[key];
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

export function str(data: Record<string, unknown>, key: string): string | null {
  const v = data[key];
  return typeof v === "string" ? v : null;
}

export function bool(data: Record<string, unknown>, key: string): boolean | null {
  const v = data[key];
  return typeof v === "boolean" ? v : null;
}

/** Array of strings (e.g. heal.proposal `files`). Non-string entries are
 *  dropped rather than failing the whole field. */
export function strArr(data: Record<string, unknown>, key: string): string[] | null {
  const v = data[key];
  if (!Array.isArray(v)) return null;
  return v.filter((x): x is string => typeof x === "string");
}

/* ------------------------------------------------------------------------ *
 * ANSWER ANNOTATIONS (anthropic.rs `answers` module / answer.annotated).      *
 *                                                                            *
 * The daemon's run_pipeline emits `answer.annotated` per turn, built by       *
 * anthropic::answer_annotation_telemetry(cite_on, confidence_on, sources,     *
 * confidence). It is the HONEST, SECRET-FREE provenance of an answer:         *
 *   - `sources` are the REAL tool-result citations that actually fed the turn  *
 *     (docsearch/unified/recall/episodic/web/integration reads) — never        *
 *     fabricated. Each carries {source (tool name), citation (the real         *
 *     locator, e.g. "indexed files" / "stored memory" / "past episodes" / a    *
 *     URL), snippet (a bounded real tool-output snippet)}.                      *
 *   - `from_my_knowledge` is true IFF cite is ON and NO source was consulted   *
 *     this turn — the honest "from my own knowledge" label, NEVER a fake cite.  *
 *   - `confidence` is the model's SELF-REPORT (a gated prompt asks for it):     *
 *     {level: grounded|inferred|uncertain, reason}. PLUMBING only — the         *
 *     calibration is runtime/model-behavior-gated and is NOT a measured score.  *
 *   - `cite_on` / `confidence_on` echo the [answers] gates (both SHIP OFF).     *
 *                                                                              *
 * Both gates OFF (the shipped default) => `sources` empty + `confidence` null  *
 * + from_my_knowledge false, so the HUD renders NOTHING. The wire carries ONLY *
 * the real locators/snippets the persona already shows + the parsed            *
 * self-report — never an embedding/audio/secret.                                *
 * ------------------------------------------------------------------------ */

/** The model's self-reported confidence levels (anthropic.rs ConfidenceLevel,
 *  lowercase on the wire). Kept as a closed union; an unknown level is dropped
 *  by the parser (the panel never renders an unrecognized badge). */
export type ConfidenceLevel = "grounded" | "inferred" | "uncertain";

/** One REAL source that fed the answer (answer_annotation_telemetry `sources[]`
 *  / anthropic.rs AnswerSource). `source` is the tool name (e.g. "doc_search",
 *  "episodic_recall"); `citation` is the real locator the daemon already shows
 *  ("indexed files", "past episodes", a URL); `snippet` is a bounded real
 *  tool-output snippet. NEVER an embedding/audio/secret. */
export interface AnswerSourceCite {
  source: string;
  citation: string;
  snippet: string;
}

/** The model's parsed confidence self-report (answer.annotated `confidence`).
 *  `level` is one of the closed set; `reason` is the one-line why. This is the
 *  model's OWN report — plumbing only, NOT a measured calibration score. */
export interface AnswerConfidence {
  level: ConfidenceLevel;
  reason: string;
}

/** A complete, defensively-parsed `answer.annotated` payload — the HUD's honest
 *  view of one answer's provenance. `citeOn`/`confidenceOn` echo the gates;
 *  `fromMyKnowledge` is the honest "no retrieval" label; `sources` are the real
 *  recorded citations; `confidence` is the model's self-report (null when off or
 *  unparsed). With both gates off this is empty sources + null confidence + the
 *  flags false, so the panel renders nothing. SECRET-FREE by construction. */
export interface AnswerAnnotation {
  citeOn: boolean;
  confidenceOn: boolean;
  fromMyKnowledge: boolean;
  sources: AnswerSourceCite[];
  confidence: AnswerConfidence | null;
}

/** Coerce one untrusted source object into an AnswerSourceCite, or null if it
 *  lacks BOTH a usable `source` (the tool name) AND a `citation` (the real
 *  locator) — a source with nothing to attribute or point at is NOT a real
 *  citation, so it is dropped, never fabricated. `snippet` defaults to "". Only
 *  the three honest fields are read — never an embedding/audio/secret. Never
 *  throws. */
function coerceAnswerSource(o: Record<string, unknown>): AnswerSourceCite | null {
  const source = str(o, "source");
  if (source === null || source.length === 0) return null;
  const citation = str(o, "citation");
  if (citation === null || citation.length === 0) return null;
  return { source, citation, snippet: str(o, "snippet") ?? "" };
}

/** Parse the `confidence` sub-object into an AnswerConfidence, or null when it
 *  is absent (confidence off / unparsed) OR carries an unknown `level` — the
 *  panel never renders an unrecognized confidence badge, and a missing confidence
 *  is the honest "no self-report this turn" rather than a fabricated one. `reason`
 *  defaults to "" when absent. Never throws. */
function parseAnswerConfidence(data: Record<string, unknown>): AnswerConfidence | null {
  const c = isPlainObject(data["confidence"]) ? data["confidence"] : null;
  if (c === null) return null;
  const level = str(c, "level");
  if (level !== "grounded" && level !== "inferred" && level !== "uncertain") {
    return null;
  }
  return { level, reason: str(c, "reason") ?? "" };
}

/** Parse an `answer.annotated` payload into an AnswerAnnotation. `sources` are
 *  coerced item-by-item (a source with no tool name or no real locator is
 *  dropped — never fabricated). `confidence` is parsed defensively (null when
 *  off/unparsed/unknown-level). NEVER returns null — an answer.annotated event
 *  always yields an honest annotation; with both gates off it is the empty
 *  (renders-nothing) shape. `fromMyKnowledge` is taken straight from the daemon
 *  (true IFF cite on AND no sources), echoed verbatim so the HUD's honest copy
 *  matches the daemon's. SECRET-FREE: only the real locators/snippets +
 *  self-report are read. Never throws. */
export function parseAnswerAnnotation(data: Record<string, unknown>): AnswerAnnotation {
  const rawSources = data["sources"];
  const sources = Array.isArray(rawSources)
    ? rawSources
        .filter(isPlainObject)
        .map(coerceAnswerSource)
        .filter((s): s is AnswerSourceCite => s !== null)
    : [];
  return {
    citeOn: bool(data, "cite_on") ?? false,
    confidenceOn: bool(data, "confidence_on") ?? false,
    fromMyKnowledge: bool(data, "from_my_knowledge") ?? false,
    sources,
    confidence: parseAnswerConfidence(data),
  };
}

/** Human label + tooltip for a confidence level — the model's self-report,
 *  honestly framed (NOT a measured score). Shared by the panel so the copy is
 *  unit-testable and consistent. */
export function confidenceLabel(level: ConfidenceLevel): string {
  switch (level) {
    case "grounded":
      return "GROUNDED";
    case "inferred":
      return "INFERRED";
    case "uncertain":
      return "UNCERTAIN";
  }
}

/** True when an annotation has NOTHING to render — no real sources, not the
 *  from-my-knowledge label, and no confidence self-report. This is the shipped
 *  default (both gates off) and the panel uses it to render nothing rather than
 *  an empty shell. */
export function answerAnnotationIsEmpty(a: AnswerAnnotation): boolean {
  return (
    a.sources.length === 0 &&
    !a.fromMyKnowledge &&
    a.confidence === null
  );
}

/* ------------------------------------------------------------------------ *
 * SELF-VERIFICATION OUTCOME (anthropic.rs `verify` module / answer.verified).  *
 *                                                                            *
 * The daemon's run_pipeline emits `answer.verified` per turn, built by         *
 * anthropic::verify_telemetry(verify_on, current_outcome()). It is the          *
 * SECRET-FREE outcome of the OPTIONAL second self-check pass ([answers].verify, *
 * which SHIPS OFF). The pass — when on AND the turn is important enough to gate  *
 * in — asks the model ONCE to critique its own DRAFT answer against the REAL     *
 * sources that turn used, and (at most ONCE) revises it. The wire carries ONLY: *
 *   - `verify_on`  — whether the [answers].verify gate is on (echoes config).    *
 *   - `outcome`    — the per-turn token: "off" (gate off / pass did not run),    *
 *     "verified-clean" (the self-check flagged nothing), "revised" (the          *
 *     self-check corrected/qualified the answer), or "flagged" (the self-check   *
 *     raised an unresolved concern, annotated onto the answer as a caveat).      *
 *   - `badge`      — the HUD label (null for "off" => render NOTHING; else       *
 *     VERIFIED / REVISED / FLAGGED).                                             *
 *   - `note`       — HONEST copy: a second self-check against the sources used;  *
 *     it REDUCES — does NOT eliminate — errors; runs only on important turns; at *
 *     most one critique + one revise; ships OFF by default.                      *
 *                                                                              *
 * HONESTY: a second self-check REDUCES hallucination on important turns — it is  *
 * NOT a correctness guarantee. VERIFIED means "the self-check found nothing to   *
 * flag", NOT "guaranteed correct". NO flagged-claim text (that rides the answer  *
 * itself when flagged), NO content beyond the answer, NO embedding/audio/secret. *
 * With the gate OFF (the shipped default) outcome === "off" + badge === null, so *
 * the HUD renders NOTHING and today's behavior is byte-for-byte unchanged.       *
 * ------------------------------------------------------------------------ */

/** The per-turn self-verification outcome token (anthropic.rs
 *  VerifyOutcome::as_str, on the wire). Closed union; an unknown token is treated
 *  as "off" by the parser so the panel never renders an unrecognized badge. */
export type VerifyOutcomeToken = "off" | "verified-clean" | "revised" | "flagged";

/** The HUD badge label for a self-verification outcome (anthropic.rs
 *  VerifyOutcome::badge). Null for "off" — nothing to render. */
export type VerifyBadge = "VERIFIED" | "REVISED" | "FLAGGED";

/** A complete, defensively-parsed `answer.verified` payload — the HUD's honest
 *  view of the second-self-check outcome for the most recent answer. `verifyOn`
 *  echoes the [answers].verify gate; `outcome` is the per-turn token; `badge` is
 *  the label (null => render nothing); `note` is the honest copy. With the gate
 *  OFF (shipped default) this is outcome "off" + null badge, so the panel renders
 *  nothing. SECRET-FREE by construction — never the flagged-claim text, never any
 *  content beyond the answer, never an embedding/audio/secret. */
export interface VerifyStatus {
  verifyOn: boolean;
  outcome: VerifyOutcomeToken;
  badge: VerifyBadge | null;
  note: string;
}

/** The badge the HUD renders for a given outcome token — the SINGLE source of
 *  truth, derived from the OUTCOME (not the wire `badge`) so a malformed/spoofed
 *  badge can never disagree with the outcome. "off" => null (render nothing). */
export function verifyBadgeFor(outcome: VerifyOutcomeToken): VerifyBadge | null {
  switch (outcome) {
    case "off":
      return null;
    case "verified-clean":
      return "VERIFIED";
    case "revised":
      return "REVISED";
    case "flagged":
      return "FLAGGED";
  }
}

/** Parse an `answer.verified` payload into a VerifyStatus. The `outcome` token is
 *  validated against the closed set — anything else (absent / unknown / junk)
 *  collapses to "off" so the panel renders nothing rather than an unrecognized
 *  badge. The `badge` is DERIVED from the validated outcome (never trusted from
 *  the wire) so it can never disagree. `note` defaults to "" when absent. NEVER
 *  returns null — an answer.verified event always yields an honest status; with
 *  the gate off it is the "off" (renders-nothing) shape. SECRET-FREE: only the
 *  gate flag, the outcome token, and the honest note are read — never the
 *  flagged-claim text or any content/embedding/audio. Never throws. */
export function parseVerifyStatus(data: Record<string, unknown>): VerifyStatus {
  const raw = str(data, "outcome");
  const outcome: VerifyOutcomeToken =
    raw === "verified-clean" || raw === "revised" || raw === "flagged"
      ? raw
      : "off";
  return {
    verifyOn: bool(data, "verify_on") ?? false,
    outcome,
    badge: verifyBadgeFor(outcome),
    note: str(data, "note") ?? "",
  };
}

/** True when a verify status has NOTHING to render — the gate is off OR the pass
 *  did not run this turn (outcome "off" => null badge). This is the shipped
 *  default ([answers].verify off) and the panel uses it to render nothing rather
 *  than an empty shell. */
export function verifyStatusIsEmpty(v: VerifyStatus): boolean {
  return v.badge === null;
}

/* ------------------------------------------------------------------------ *
 * TOOL-RESULT CROSS-CHECK OUTCOME (#21 — anthropic.rs `crosscheck` module /  *
 * answer.cross_checked). SIBLING of the verify surface above.                *
 *                                                                            *
 * Before a tool result is SURFACED to the user as fact (or a consequential   *
 * action is built from it), the daemon — when [answers].cross_check is on —   *
 * runs a BOUNDED plausibility cross-check: deterministic sanity checks first  *
 * (shape/range/contradiction/empty-vs-claimed/citation-present), then an      *
 * OPTIONAL single bounded model "does this look right?" pass for important     *
 * results. A failed check DOWNGRADES confidence (#8) and FLAGS the result —    *
 * it NEVER silently trusts and NEVER removes a consequential action's existing *
 * confirmation gate (it can only ADD caution).                                 *
 *                                                                            *
 * The daemon's run_pipeline emits `answer.cross_checked` per turn, built by    *
 * anthropic::cross_check_badge_telemetry(cross_check_on, current_outcome()).   *
 * The wire carries ONLY (SECRET-FREE — never the raw tool result; the flag     *
 * reasons + caveat ride the answer text itself):                               *
 *   - `cross_check_on` — whether the [answers].cross_check gate is on.         *
 *   - `outcome`        — the per-turn token: "off" (gate off / did not run),   *
 *     "plausible" (nothing tripped — NOT "correct", just nothing flagged), or  *
 *     "flagged" (a check tripped: implausible/empty/uncited/contradictory —    *
 *     confidence was downgraded + the result flagged).                         *
 *   - `badge`          — the HUD label (null for "off" => render NOTHING; else  *
 *     CHECKED / UNVERIFIED).                                                    *
 *   - `note`           — HONEST copy: it only DOWNGRADES + flags, NEVER removes *
 *     a confirmation gate, is NOT a correctness guarantee, ships OFF.           *
 *                                                                            *
 * HONESTY: CHECKED means "the plausibility checks found nothing to flag", NOT  *
 * "guaranteed correct"; UNVERIFIED means a check tripped and confidence was    *
 * downgraded — never that the gate was removed. With the gate OFF (the shipped *
 * default) outcome === "off" + badge === null, so the HUD renders NOTHING.     *
 * ------------------------------------------------------------------------ */

/** The per-turn cross-check outcome token (anthropic.rs CrossCheckOutcome::as_str,
 *  on the wire). Closed union; an unknown token collapses to "off" in the parser
 *  so the panel never renders an unrecognized badge. */
export type CrossCheckOutcomeToken = "off" | "plausible" | "flagged";

/** The HUD badge label for a cross-check outcome (anthropic.rs
 *  CrossCheckOutcome::badge). Null for "off" — nothing to render. */
export type CrossCheckBadge = "CHECKED" | "UNVERIFIED";

/** A complete, defensively-parsed `answer.cross_checked` payload — the HUD's
 *  honest view of the tool-result plausibility cross-check for the most recent
 *  answer. `crossCheckOn` echoes the [answers].cross_check gate; `outcome` is the
 *  per-turn token; `badge` is the label (null => render nothing); `note` is the
 *  honest copy. With the gate OFF (shipped default) this is outcome "off" + null
 *  badge, so the panel renders nothing. SECRET-FREE by construction — never the
 *  raw tool result, never the flag-reason text (that rides the answer when
 *  flagged), never an embedding/audio/secret. */
export interface CrossCheckStatus {
  crossCheckOn: boolean;
  outcome: CrossCheckOutcomeToken;
  badge: CrossCheckBadge | null;
  note: string;
}

/** The badge the HUD renders for a given cross-check outcome — the SINGLE source
 *  of truth, derived from the OUTCOME (not the wire `badge`) so a malformed/spoofed
 *  badge can never disagree with the outcome. "off" => null (render nothing). */
export function crossCheckBadgeFor(
  outcome: CrossCheckOutcomeToken,
): CrossCheckBadge | null {
  switch (outcome) {
    case "off":
      return null;
    case "plausible":
      return "CHECKED";
    case "flagged":
      return "UNVERIFIED";
  }
}

/** Parse an `answer.cross_checked` payload into a CrossCheckStatus. The `outcome`
 *  token is validated against the closed set — anything else (absent / unknown /
 *  junk) collapses to "off" so the panel renders nothing rather than an
 *  unrecognized badge. The `badge` is DERIVED from the validated outcome (never
 *  trusted from the wire) so it can never disagree. `note` defaults to "" when
 *  absent. NEVER returns null — an answer.cross_checked event always yields an
 *  honest status; with the gate off it is the "off" (renders-nothing) shape.
 *  SECRET-FREE: only the gate flag, the outcome token, and the honest note are
 *  read — never the raw tool result, never the flag-reason text, never any
 *  content/embedding/audio. Never throws. */
export function parseCrossCheckStatus(
  data: Record<string, unknown>,
): CrossCheckStatus {
  const raw = str(data, "outcome");
  const outcome: CrossCheckOutcomeToken =
    raw === "plausible" || raw === "flagged" ? raw : "off";
  return {
    crossCheckOn: bool(data, "cross_check_on") ?? false,
    outcome,
    badge: crossCheckBadgeFor(outcome),
    note: str(data, "note") ?? "",
  };
}

/** True when a cross-check status has NOTHING to render — the gate is off OR the
 *  cross-check did not run this turn (outcome "off" => null badge). This is the
 *  shipped default ([answers].cross_check off) and the panel uses it to render
 *  nothing rather than an empty shell. */
export function crossCheckStatusIsEmpty(v: CrossCheckStatus): boolean {
  return v.badge === null;
}

/* ------------------------------------------------------------------------ *
 * MULTI-MODEL DEBATE OUTCOME (#22 — anthropic.rs `debate` module /           *
 * answer.debated). SIBLING of the verify + cross-check surfaces above.       *
 *                                                                            *
 * For GATED high-stakes asks ONLY (a conservative `should_debate` predicate + *
 * the [answers].debate flag, OFF-default), the daemon runs TWO brains on the   *
 * same question (local + cloud, or fast + heavy), then RECONCILES — bounded to *
 * at most TWO model calls. The daemon's run_pipeline emits `answer.debated`     *
 * per turn, built by anthropic::debate_badge_telemetry(debate_on,              *
 * current_outcome()). The wire carries ONLY (SECRET-FREE — never the raw        *
 * answers; when the brains disagree BOTH answers ride the answer text itself):  *
 *   - `debate_on` — whether the [answers].debate gate is on.                   *
 *   - `outcome`   — the per-turn token: "off" (gate off / should_debate        *
 *     declined — an ORDINARY turn never debates), "agree" (both brains          *
 *     substantively agreed => confidence RAISED), "disagree" (the brains         *
 *     disagreed => BOTH answers surfaced + flagged, never silently picked or     *
 *     averaged into a fake consensus), or "fallback" (the second brain was       *
 *     unavailable => the single answer stands + it is stated no second opinion    *
 *     was obtained — runtime-gated, no fabricated agreement).                    *
 *   - `badge`     — the HUD label (null for "off" => render NOTHING; else        *
 *     CORROBORATED / DISPUTED / ONE-MODEL).                                      *
 *   - `note`      — HONEST copy: agreement raises, disagreement surfaces both    *
 *     (never picked/averaged), fallback says so; at most two calls, ships OFF.   *
 *                                                                            *
 * HONESTY: the second-brain quality gain is RUNTIME-gated (only when the brain  *
 * is actually available — else "fallback", stated, never a fabricated           *
 * consensus). Disagreement is SURFACED (DISPUTED), never hidden. With the gate   *
 * OFF (the shipped default), and on every ordinary turn, outcome === "off" +     *
 * badge === null, so the HUD renders NOTHING.                                    *
 * ------------------------------------------------------------------------ */

/** The per-turn debate outcome token (anthropic.rs DebateOutcome::as_str, on the
 *  wire). Closed union; an unknown token collapses to "off" in the parser so the
 *  panel never renders an unrecognized badge. */
export type DebateOutcomeToken = "off" | "agree" | "disagree" | "fallback";

/** The HUD badge label for a debate outcome (anthropic.rs DebateOutcome::badge).
 *  Null for "off" — nothing to render. */
export type DebateBadge = "CORROBORATED" | "DISPUTED" | "ONE-MODEL";

/** A complete, defensively-parsed `answer.debated` payload — the HUD's honest
 *  view of the multi-model debate outcome for the most recent answer. `debateOn`
 *  echoes the [answers].debate gate; `outcome` is the per-turn token; `badge` is
 *  the label (null => render nothing); `note` is the honest copy. With the gate
 *  OFF (shipped default), and on every ordinary turn, this is outcome "off" + null
 *  badge, so the panel renders nothing. SECRET-FREE by construction — never the
 *  raw answers (when the brains disagree BOTH answers ride the answer text), never
 *  an embedding/audio/secret. */
export interface DebateStatus {
  debateOn: boolean;
  outcome: DebateOutcomeToken;
  badge: DebateBadge | null;
  note: string;
}

/** The badge the HUD renders for a given debate outcome — the SINGLE source of
 *  truth, derived from the OUTCOME (not the wire `badge`) so a malformed/spoofed
 *  badge can never disagree with the outcome. "off" => null (render nothing). */
export function debateBadgeFor(outcome: DebateOutcomeToken): DebateBadge | null {
  switch (outcome) {
    case "off":
      return null;
    case "agree":
      return "CORROBORATED";
    case "disagree":
      return "DISPUTED";
    case "fallback":
      return "ONE-MODEL";
  }
}

/** Parse an `answer.debated` payload into a DebateStatus. The `outcome` token is
 *  validated against the closed set — anything else (absent / unknown / junk)
 *  collapses to "off" so the panel renders nothing rather than an unrecognized
 *  badge. The `badge` is DERIVED from the validated outcome (never trusted from
 *  the wire) so it can never disagree. `note` defaults to "" when absent. NEVER
 *  returns null — an answer.debated event always yields an honest status; with the
 *  gate off (and on every ordinary turn) it is the "off" (renders-nothing) shape.
 *  SECRET-FREE: only the gate flag, the outcome token, and the honest note are
 *  read — never the raw answers, never any content/embedding/audio. Never throws. */
export function parseDebateStatus(data: Record<string, unknown>): DebateStatus {
  const raw = str(data, "outcome");
  const outcome: DebateOutcomeToken =
    raw === "agree" || raw === "disagree" || raw === "fallback" ? raw : "off";
  return {
    debateOn: bool(data, "debate_on") ?? false,
    outcome,
    badge: debateBadgeFor(outcome),
    note: str(data, "note") ?? "",
  };
}

/** True when a debate status has NOTHING to render — the gate is off OR the debate
 *  did not run this turn (an ordinary turn; outcome "off" => null badge). This is
 *  the shipped default ([answers].debate off) and the panel uses it to render
 *  nothing rather than an empty shell. */
export function debateStatusIsEmpty(v: DebateStatus): boolean {
  return v.badge === null;
}

/* ======================================================================== *
 * CONSEQUENTIAL GATE — AUDIT LOG + POLICY                                    *
 *                                                                            *
 * The crown-jewel gate's accountability surface (daemon/src/audit.rs +       *
 * daemon/src/policy.rs). Two read snapshots + the live chokepoint events.    *
 *                                                                            *
 *  - `system / audit.snapshot` : the daemon's AuditLog read API folded to    *
 *    the wire — recent(n) entries (newest-first) + verify_chain() status +   *
 *    len() + enabled. Every AuditEntry is SECRET-FREE by construction on the *
 *    daemon side (target is REDACTED twice; the raw tool input never lands    *
 *    in the log). The parser below carries that contract forward: it surfaces *
 *    ONLY {seq,ts,agent,tool,target,decision,outcome} and the chain status — *
 *    NOT prev_hash/entry_hash (internal chain bytes the operator never needs  *
 *    to read), so even a malformed payload can never smuggle a secret field.  *
 *  - `system / policy.snapshot` : the daemon's PolicyStore::rules() folded to *
 *    the wire — the user-set per-action rules (tool [+agent] [+recipient] ->  *
 *    always|never|ask) in the store's deterministic order, plus the on/off    *
 *    posture. SHIPPED-EMPTY default: rules=[] (ASK everywhere), so the editor *
 *    renders the honest "no rules — ASK everywhere" state.                    *
 *  - Live chokepoint events (`policy.blocked` / `policy.auto_approved`,       *
 *    `confirm.parked`, `audit.truncated`) fold into a rolling live timeline   *
 *    so the panel reacts immediately between snapshots.                       *
 *                                                                            *
 * HONESTY (surfaced in the panel copy, pinned here): the audit log is        *
 * tamper-EVIDENT (hash-chained), NOT tamper-PROOF — a root attacker who       *
 * rewrites the WHOLE on-disk chain still verifies. ALWAYS is a deliberate,    *
 * logged, MASTER-GATED user loosening (inert when the master switch is OFF).  *
 * NEVER always wins. Policies are USER-SET ONLY (no agent/model write path).  *
 * The master switch + voice-id + confirmation remain the hard backstop.      *
 * ======================================================================== */

/** The policy decision tokens (daemon Decision::as_str). NEVER hard-blocks,
 *  ALWAYS auto-approves (only when the master switch is ON + voice-id allows —
 *  enforced daemon-side, NOT here), ASK is the default park/confirm flow. */
export type PolicyDecision = "always" | "never" | "ask";

/** Coerce an untrusted decision token to a known one, defaulting to the SAFE
 *  "ask" (the default park/confirm) for anything unrecognized — a junk token
 *  must never read as a loosening (always) or a different block. */
export function coercePolicyDecision(v: unknown): PolicyDecision {
  return v === "always" || v === "never" || v === "ask" ? v : "ask";
}

/** The audit outcome tokens (daemon Outcome::as_str). The HUD treats any
 *  unknown token as an opaque string (forward-tolerant) but knows these. */
export type AuditOutcome =
  | "proposed"
  | "parked"
  | "blocked_by_policy"
  | "auto_approved_by_policy"
  | "always_inert_master_off"
  | "confirmed"
  | "denied"
  | "executed"
  | "dry_run";

/** One audit-log entry as surfaced to the HUD — the SECRET-FREE subset of the
 *  daemon's AuditEntry. `target` is the daemon's ALREADY-redacted target summary
 *  (recipient/channel/device/amount), NEVER the raw input. The internal chain
 *  bytes (prev_hash/entry_hash) are deliberately NOT carried — the operator
 *  reads the decision/outcome timeline + the single chain-OK verdict, not the
 *  raw hashes. */
export interface AuditEntry {
  seq: number;
  ts: string;
  agent: string;
  tool: string;
  target: string;
  decision: PolicyDecision;
  outcome: string; // a known AuditOutcome token, or an opaque future one
}

/** The chain-integrity verdict (daemon ChainStatus). `ok` is the chain-OK
 *  indicator; when broken, `brokenSeq`/`reason` say WHERE + WHY (secret-free).
 *  `count` is how many entries verified. */
export interface ChainStatus {
  ok: boolean;
  count: number;
  brokenSeq: number | null;
  reason: string | null;
}

/** The whole AUDIT surface (audit.snapshot). `enabled` is the [audit] on/off
 *  posture; `total` is the full bounded length (entries may exceed the shown
 *  recent window); `truncated` notes that a prune has re-rooted the chain (the
 *  surviving suffix still verifies). `entries` is newest-first. */
export interface AuditSnapshot {
  enabled: boolean;
  total: number;
  truncated: boolean;
  chain: ChainStatus;
  entries: AuditEntry[];
}

/** One user-set policy rule as surfaced to the HUD — mirrors the daemon's
 *  PolicyRule{scope,decision}. `agent`/`recipient` are the OPTIONAL narrowing
 *  scope (null = "any"); `tool` is always present (a rule is always anchored to
 *  a specific tool — there is no blanket all-tools rule). */
export interface PolicyRule {
  tool: string;
  agent: string | null;
  recipient: string | null;
  decision: PolicyDecision;
}

/** The whole POLICY surface (policy.snapshot). `enabled` is the [policy] on/off
 *  posture; `rules` is the user-set store in the daemon's deterministic order.
 *  SHIPPED-EMPTY default: rules=[] (ASK everywhere). USER-SET ONLY — the daemon
 *  never writes a rule from the tool loop, and this snapshot is read-only on the
 *  HUD side (writes go through the command channel, not by mutating this). */
export interface PolicySnapshot {
  enabled: boolean;
  rules: PolicyRule[];
}

/** Coerce one untrusted audit entry, or null if it lacks the structural anchors
 *  (a usable tool + a finite seq). Surfaces ONLY the secret-free subset — any
 *  extra field on the wire (a stray prev_hash, a token-shaped field) is IGNORED,
 *  so the panel can never render a secret. `decision` defaults to the safe "ask";
 *  `outcome` is carried as an opaque string (forward-tolerant). Never throws. */
function coerceAuditEntry(o: Record<string, unknown>): AuditEntry | null {
  const tool = str(o, "tool");
  if (tool === null || tool.length === 0) return null;
  const seq = num(o, "seq");
  if (seq === null) return null;
  return {
    seq,
    ts: str(o, "ts") ?? "",
    agent: str(o, "agent") ?? "",
    tool,
    // The daemon field is `target_redacted` (already secret-free); we surface it
    // as `target`. A missing target reads as the honest empty summary, never a
    // fallback that could echo other fields.
    target: str(o, "target_redacted") ?? str(o, "target") ?? "",
    decision: coercePolicyDecision(o["decision"]),
    outcome: str(o, "outcome") ?? "",
  };
}

/** Parse the chain-status sub-object of audit.snapshot. Defaults to a SAFE
 *  "not-ok / unknown" verdict when absent or malformed — a missing/garbled chain
 *  status must never read as a green chain-OK (fail toward the honest "can't
 *  confirm" state, never toward a false all-clear). */
function coerceChainStatus(v: unknown): ChainStatus {
  if (!isPlainObject(v)) {
    return { ok: false, count: 0, brokenSeq: null, reason: "no chain status" };
  }
  const ok = bool(v, "ok") ?? false;
  const count = num(v, "count") ?? 0;
  if (ok) return { ok: true, count, brokenSeq: null, reason: null };
  return {
    ok: false,
    count,
    brokenSeq: num(v, "broken_seq"),
    reason: str(v, "reason") ?? "chain verification failed",
  };
}

/** Parse an audit.snapshot payload. NEVER returns null — an audit frame always
 *  yields a (possibly empty) snapshot so the panel renders the honest current
 *  state (off / empty / a verified or broken chain) rather than a stale one.
 *  `enabled` defaults to false (shipped posture), the chain defaults to the safe
 *  not-ok verdict, entries are coerced item-by-item (junk dropped). NEVER carries
 *  a secret (only the secret-free subset survives). Never throws on junk. */
export function parseAuditSnapshot(data: Record<string, unknown>): AuditSnapshot {
  const rawEntries = data["entries"];
  const entries = Array.isArray(rawEntries)
    ? rawEntries
        .filter(isPlainObject)
        .map(coerceAuditEntry)
        .filter((e): e is AuditEntry => e !== null)
    : [];
  return {
    enabled: bool(data, "enabled") ?? false,
    total: num(data, "total") ?? entries.length,
    truncated: bool(data, "truncated") ?? false,
    chain: coerceChainStatus(data["chain"]),
    entries,
  };
}

/** Coerce one untrusted policy rule, or null if it lacks the structural anchor
 *  (a usable tool name — a rule is always anchored to a specific tool). An unset
 *  agent/recipient reads as null ("any"). `decision` defaults to the safe "ask"
 *  (a junk decision must never read as an "always" loosening). Never throws. */
function coercePolicyRule(o: Record<string, unknown>): PolicyRule | null {
  // The daemon nests the matcher under `scope`; tolerate a flat shape too.
  const scope = isPlainObject(o["scope"]) ? (o["scope"] as Record<string, unknown>) : o;
  const tool = str(scope, "tool");
  if (tool === null || tool.length === 0) return null;
  return {
    tool,
    agent: str(scope, "agent"),
    recipient: str(scope, "recipient"),
    decision: coercePolicyDecision(o["decision"]),
  };
}

/** Parse a policy.snapshot payload. NEVER returns null — a policy frame always
 *  yields a (possibly empty) snapshot so the editor renders the honest current
 *  state (off / empty "ASK everywhere" / the user's rules) rather than a stale
 *  one. SHIPPED-EMPTY default: enabled=false, rules=[]. Rules are coerced
 *  item-by-item (junk dropped); a junk decision reads as the safe "ask", never a
 *  loosening. Never throws on junk. */
export function parsePolicySnapshot(data: Record<string, unknown>): PolicySnapshot {
  const rawRules = data["rules"];
  const rules = Array.isArray(rawRules)
    ? rawRules
        .filter(isPlainObject)
        .map(coercePolicyRule)
        .filter((r): r is PolicyRule => r !== null)
    : [];
  return { enabled: bool(data, "enabled") ?? false, rules };
}

/** One live consequential-gate event folded into the rolling timeline from the
 *  daemon's CHOKEPOINT telemetry (`policy.blocked` / `policy.auto_approved` /
 *  `confirm.parked`), BETWEEN authoritative audit.snapshot frames. This is the
 *  "react immediately" surface — `kind` is the chokepoint verdict, derived ONLY
 *  from the event name + its secret-free {tool,agent} (and an optional mcp/via
 *  marker). SECRET-FREE: the chokepoint events carry no target/input. */
export interface LiveGateEvent {
  kind: "blocked" | "auto_approved" | "parked";
  tool: string;
  agent: string;
  /** "mcp" when the chokepoint was an MCP tool, "selector" for a standing
   *  mission, else null (a built-in tool). Secret-free routing context only. */
  via: string | null;
  ts: string;
  /** A HUD-local monotonic key (the reducer seq) so React lists are stable even
   *  when two events share a ts. */
  seq: number;
}

/** Fold a chokepoint telemetry event into a LiveGateEvent, or null if it is not
 *  a recognized gate event (so the reducer can ignore it). `tool` defaults to
 *  "(unknown)" rather than dropping the event — a gate decision firing is worth
 *  surfacing even if the tool name is malformed. SECRET-FREE by construction
 *  (the chokepoint payloads carry only tool/agent + an mcp/via marker). */
export function liveGateEventFrom(
  event: string,
  data: Record<string, unknown>,
  ts: string,
  seq: number,
): LiveGateEvent | null {
  let kind: LiveGateEvent["kind"];
  if (event === "policy.blocked") kind = "blocked";
  else if (event === "policy.auto_approved") kind = "auto_approved";
  else if (event === "confirm.parked") kind = "parked";
  else return null;
  const via = (bool(data, "mcp") ?? false) ? "mcp" : str(data, "via");
  return {
    kind,
    tool: str(data, "tool") ?? "(unknown)",
    agent: str(data, "agent") ?? "",
    via,
    ts,
    seq,
  };
}

/* ------------------------------------------------------------------------ *
 * RESEARCH NOTEBOOKS (daemon/src/notebook.rs NotebookCard, emitted from      *
 * router.rs as `notebook.card`).                                             *
 *                                                                            *
 * A notebook voice command ("save this research" / "show my research        *
 * notebook on X" / "what have I researched" / "forget my research on X")     *
 * ran. The daemon PERSISTS a real SAGE run that ALREADY happened and READS   *
 * runs that were really saved — it never fetches/fabricates here. The wire   *
 * carries the verb plus an OPTIONAL card with the surfaced run's already-    *
 * redacted snippet and its REAL fetched-source citations (run-local id +     *
 * title + url) — exactly the persisted, grounded ones, NEVER an invented     *
 * source, NEVER raw content, NEVER a secret.                                 *
 *                                                                            *
 * HONESTY (mirrors the daemon, surfaced verbatim by the panel):              *
 *   - citations are the REAL fetched sources the run was grounded in; the    *
 *     parser drops any citation with no usable url AND no title — there is    *
 *     nothing to point at, so it is never fabricated. An empty citations[] is *
 *     the honest "this run had no grounded sources".                          *
 *   - the snippet is the already-redacted synthesized text (bounded); the    *
 *     spoken reply holds the full text. The HUD shows only what the user      *
 *     already owns.                                                           *
 *   - save_none/forget_none/error carry NO card (card === null) — there is   *
 *     nothing real to surface, so the panel shows nothing new (READ-ONLY).    *
 *   - an honest-empty revisit carries a card with runCount 0 + empty         *
 *     citations + "" snippet — surfaced as the honest "no saved runs yet".    *
 * ------------------------------------------------------------------------ */

/** The set of notebook verbs the panel recognizes. An unknown verb is dropped
 *  (the event is ignored) so a malformed/spoofed verb never renders. */
export type NotebookVerb =
  | "saved"
  | "revisit"
  | "list"
  | "forget"
  | "save_none"
  | "forget_none"
  | "error";

/** One REAL fetched-source citation of a surfaced notebook run — the run-local
 *  source id, the page title, and the real URL the run was grounded in. Only
 *  ever built from a citation with something to point at (a url OR a title); a
 *  citation with neither is dropped (not a real citation). NEVER a secret. */
export interface NotebookCite {
  sourceId: number;
  title: string;
  url: string;
}

/** The surfaced notebook card (notebook.card `card`): the verb, the human topic,
 *  a bounded already-redacted snippet of the most-recent run, the REAL citations,
 *  and the saved-run count (or #notebooks for a list). Built only from a real
 *  persisted run/shelf — never fabricated, never raw, never a secret. */
export interface NotebookCard {
  verb: NotebookVerb;
  topic: string;
  snippet: string;
  runCount: number;
  citations: NotebookCite[];
}

/** A parsed `notebook.card` payload: the verb plus the OPTIONAL card. `card` is
 *  null for save_none/forget_none/error (nothing to surface) AND for any
 *  malformed payload where no real card can be built. NEVER returns null — a
 *  notebook.card event always yields an honest activity record. */
export interface NotebookActivity {
  verb: NotebookVerb;
  card: NotebookCard | null;
}

/** Coerce one untrusted notebook citation into a NotebookCite, or null if it has
 *  NEITHER a usable `url` NOR a `title` — a citation with nothing to point at is
 *  not a real fetched source, so it is dropped (never fabricated). Only the three
 *  honest locator fields are read — never an embedding/audio/secret. Never throws. */
function coerceNotebookCite(o: Record<string, unknown>): NotebookCite | null {
  const url = (str(o, "url") ?? "").trim();
  const title = (str(o, "title") ?? "").trim();
  if (url.length === 0 && title.length === 0) return null;
  return { sourceId: nonNegInt(o, "source_id"), title, url };
}

/** Narrow an untrusted verb string to a known NotebookVerb, or null. */
function asNotebookVerb(v: string | null): NotebookVerb | null {
  switch (v) {
    case "saved":
    case "revisit":
    case "list":
    case "forget":
    case "save_none":
    case "forget_none":
    case "error":
      return v;
    default:
      return null;
  }
}

/** Parse a `notebook.card` payload into a NotebookActivity. The verb is narrowed
 *  to the known set (an unknown verb yields card null + verb "error" — the
 *  reducer drops it). The card is built ONLY when the wire carries a `card`
 *  object whose verb is known; citations are coerced item-by-item (a citation
 *  with no url AND no title is dropped — never fabricated). SECRET-FREE: only the
 *  verb, topic, bounded snippet, run count, and real locators are read — never an
 *  embedding/audio/raw content/secret. Never throws. */
export function parseNotebookActivity(data: Record<string, unknown>): NotebookActivity {
  const verb = asNotebookVerb(str(data, "verb"));
  if (verb === null) return { verb: "error", card: null };

  const rawCard = data["card"];
  if (!isPlainObject(rawCard)) return { verb, card: null };

  // The card's own verb must also be a known one; otherwise drop the card.
  const cardVerb = asNotebookVerb(str(rawCard, "verb")) ?? verb;
  const rawCites = rawCard["citations"];
  const citations = Array.isArray(rawCites)
    ? rawCites
        .filter(isPlainObject)
        .map(coerceNotebookCite)
        .filter((c): c is NotebookCite => c !== null)
    : [];
  return {
    verb,
    card: {
      verb: cardVerb,
      topic: (str(rawCard, "topic") ?? "").trim(),
      snippet: (str(rawCard, "snippet") ?? "").trim(),
      runCount: nonNegInt(rawCard, "run_count"),
      citations,
    },
  };
}

/** A human label for what a notebook verb DID — honest, past-tense activity copy
 *  (never implies a live fetch happened here). */
export function notebookVerbLabel(verb: NotebookVerb): string {
  switch (verb) {
    case "saved":
      return "SAVED";
    case "revisit":
      return "REVISITED";
    case "list":
      return "SHELF";
    case "forget":
      return "FORGOTTEN";
    case "save_none":
      return "NOTHING TO SAVE";
    case "forget_none":
      return "NOTHING TO FORGET";
    case "error":
      return "UNAVAILABLE";
  }
}

/* ------------------------------------------------------------------------ *
 * LIFE-LOG DIGEST (daemon/src/lifelog.rs LifeLogCard, emitted from router.rs *
 * as `lifelog.digest`).                                                      *
 *                                                                            *
 * A life-log voice command ("what did I do today/this week" / "show my life  *
 * log") ran. The daemon SUMMARIZES the user's REAL recorded episodes over the *
 * agent's recall scope — every field is the episodic store's ALREADY-REDACTED *
 * output (a secret was stripped BEFORE write), bounded by the digest's caps.  *
 * It NEVER fabricates an event: an empty window rides `empty: true` with a    *
 * zero count and empty lists, surfaced as an honest empty state.             *
 *                                                                            *
 * HONESTY (mirrors the daemon, surfaced verbatim by the panel):              *
 *   - the digest summarizes YOUR real redacted episodes — bounded + forgettable, *
 *     never invented. `episodeCount` is the REAL recorded-turn count.          *
 *   - `empty` => the honest "nothing logged" state (no themes/topics/summaries). *
 *   - themes/topics/recentSummaries are the already-redacted, bounded salient   *
 *     content; a non-string entry is dropped rather than failing the field.    *
 *   - READ-ONLY: there is nothing to act on; the panel only SHOWS the digest.   *
 * ------------------------------------------------------------------------ */

/** The known life-log period labels. Anything else maps to null (the reducer
 *  drops the event) so a malformed/spoofed period never renders. */
export type LifeLogPeriod = "today" | "this week";

/** A parsed `lifelog.digest` payload: the period, the honest-empty flag, the REAL
 *  episode count, the rendered digest text, and the bounded already-redacted
 *  themes / topics / recent summaries. Built only from the daemon's redacted
 *  digest — never raw, never fabricated. */
export interface LifeLogDigest {
  period: LifeLogPeriod;
  empty: boolean;
  episodeCount: number;
  digestText: string;
  themes: string[];
  topics: string[];
  recentSummaries: string[];
}

/** Narrow an untrusted period string to a known LifeLogPeriod, or null. */
function asLifeLogPeriod(v: string | null): LifeLogPeriod | null {
  return v === "today" || v === "this week" ? v : null;
}

/** A bounded array of trimmed, non-empty strings (the digest's already-redacted
 *  themes/topics/summaries). Non-string entries are dropped; empty/whitespace
 *  entries are dropped; the result is capped to `max` so a spoofed huge list
 *  never floods the panel. Returns [] for a missing/non-array field. */
function redactedStrList(
  data: Record<string, unknown>,
  key: string,
  max: number,
): string[] {
  const v = data[key];
  if (!Array.isArray(v)) return [];
  return v
    .filter((x): x is string => typeof x === "string")
    .map((s) => s.trim())
    .filter((s) => s.length > 0)
    .slice(0, max);
}

/** How many themes/topics/summaries the panel will surface (defensive caps over
 *  the daemon's own bounds — a glance, not a dump). */
export const LIFELOG_THEME_CAP = 12;
export const LIFELOG_SUMMARY_CAP = 8;

/** Parse a `lifelog.digest` payload into a LifeLogDigest, or null when the period
 *  is not a recognized label (an unparseable digest is dropped rather than
 *  rendered with a fabricated period). `empty` is taken from the daemon; the
 *  lists are coerced to bounded redacted-string arrays (non-strings dropped).
 *  SECRET-FREE: only the period, flag, count, rendered text, and already-redacted
 *  bounded lists are read — never raw episodes/embeddings/audio/secret. Never
 *  throws. */
export function parseLifeLogDigest(
  data: Record<string, unknown>,
): LifeLogDigest | null {
  const period = asLifeLogPeriod(str(data, "period"));
  if (period === null) return null;
  return {
    period,
    empty: bool(data, "empty") ?? false,
    episodeCount: nonNegInt(data, "episode_count"),
    digestText: (str(data, "digest_text") ?? "").trim(),
    themes: redactedStrList(data, "themes", LIFELOG_THEME_CAP),
    topics: redactedStrList(data, "topics", LIFELOG_THEME_CAP),
    recentSummaries: redactedStrList(data, "recent_summaries", LIFELOG_SUMMARY_CAP),
  };
}

/** A human label for the digest period (upper-cased for the panel header). */
export function lifeLogPeriodLabel(period: LifeLogPeriod): string {
  return period === "today" ? "TODAY" : "THIS WEEK";
}

/* ------------------------------------------------------------------------ *
 * ACTION SURFACE (#25 auto-draft / #26 durable missions / #27 macros) —      *
 * the read-only HUD view of the three OFF-default, gated, wired-live action  *
 * features. The daemon emits all of these via telemetry::emit("system", …)   *
 * and EVERY payload is SECRET-FREE by construction (ids / names / intents /  *
 * subjects / counts only — never a full draft body, never a token, never a   *
 * resolved credential, never a macro's literal secret).                       *
 *                                                                            *
 * HONESTY (the lines this surface must hold, surfaced verbatim by the panel): *
 *   #25 a draft is a SUGGESTION the user reviews + sends — JARVIS NEVER       *
 *       auto-sends it. The draft module has NO send path; an actual send is a *
 *       SEPARATE explicit action that rides the EXISTING consequential gate.   *
 *       The status surface shows the subject + a bounded preview ONLY — never  *
 *       the full body, never a secret.                                         *
 *   #26 a persisted mission LOADS PAUSED on restart (never auto-runs); a       *
 *       resumed mission RE-GATES each consequential sub-task step through the  *
 *       SAME gate (the persistence carries no pre-approval). The panel shows   *
 *       id / goal / status / sub-task progress only.                           *
 *   #27 a macro stores ONLY the recorded intents/utterances (NEVER a secret/  *
 *       token/credential); a replay RE-RUNS each command through the NORMAL    *
 *       router + the gate EACH time (a consequential step is gated fresh, no   *
 *       pre-approval, no batching past the gate). The panel shows the named    *
 *       list + the last replay outcome only.                                   *
 *                                                                            *
 * All three SHIP OFF behind their own flags ([drafts].enabled,                 *
 * [missions].durable, [macros].enabled), so nothing arrives on this surface    *
 * until the operator explicitly turns a feature on. A `*.blocked {reason}`     *
 * with reason "disabled" is the inert shipped-OFF default — NOT an error — so   *
 * the parsers/reducer drop it (mirrors forge.blocked reason=disabled).         *
 * ------------------------------------------------------------------------ */

/** Defensive caps so a misbehaving/compromised daemon frame cannot overflow
 *  the status surface. The draft preview is bounded HARD (the full body never
 *  rides the wire, but the preview is still clipped here as defense in depth). */
export const DRAFT_SUBJECT_CAP = 140;
export const DRAFT_PREVIEW_CAP = 200;
export const MISSION_GOAL_CAP = 200;
export const MACRO_NAME_CAP = 80;

/** #25 — the recognized draft kinds (anthropic.rs/draft.rs `kind`). Kept as a
 *  closed union for the badge; an unknown future kind renders verbatim, so the
 *  panel is forward-tolerant rather than hiding a draft it does not recognize. */
export type DraftKind = "email_reply" | "message" | "doc" | string;

/** #25 — the lifecycle status of a draft. The ONLY status the draft module ever
 *  writes is "draft" (a draft is NEVER sent by the draft module — a send is a
 *  separate gated action). "forgotten" is folded HUD-side from draft.forgotten.
 *  A defensive parser pins this: anything that is not the literal "draft" string
 *  is coerced to "draft" so the surface can NEVER imply a draft was auto-sent. */
export type DraftStatus = "draft";

/** #25 — one PENDING draft (draft.composed). SECRET-FREE: the id, the kind, a
 *  bounded subject + preview the persona already shows — NEVER the full body,
 *  NEVER a recipient secret/token. `status` is always "draft" (the draft module
 *  has no send path). The full body lives in the daemon's pending-draft store
 *  and is reviewed by voice, never broadcast here. */
export interface PendingDraft {
  id: string;
  kind: DraftKind;
  status: DraftStatus;
  /** A bounded subject line (may be empty for a doc/message draft). */
  subject: string;
  /** A bounded preview snippet — the FIRST line(s) the persona already shows,
   *  NEVER the full body. Empty when the daemon sent none. */
  preview: string;
  ts: string; // envelope ts
}

/** Parse a draft.composed payload into a PendingDraft, or null if it lacks the
 *  structural anchor (a usable id — the panel must be able to key + later forget
 *  it). `status` is hard-pinned to "draft" regardless of what the wire said, so
 *  the surface can never render a draft as sent. The preview is clipped HARD
 *  (the full body never rides the wire; this is defense in depth). SECRET-FREE.
 *  Never throws. */
export function parseDraftComposed(
  data: Record<string, unknown>,
  ts: string,
): PendingDraft | null {
  const id = str(data, "id");
  if (id === null || id.length === 0) return null;
  const kind = str(data, "kind");
  return {
    id,
    // The draft module only ever produces a "draft"; pin it so a malformed/
    // hostile status (e.g. a fabricated "sent") can NEVER reach the surface.
    status: "draft",
    kind: kind !== null && kind.length > 0 ? kind : "draft",
    subject: (str(data, "subject") ?? "").slice(0, DRAFT_SUBJECT_CAP),
    preview: (str(data, "preview") ?? "").slice(0, DRAFT_PREVIEW_CAP),
    ts,
  };
}

/** A human label for a draft kind (for the panel badge). */
export function draftKindLabel(kind: DraftKind): string {
  if (kind === "email_reply") return "EMAIL REPLY";
  if (kind === "message") return "MESSAGE";
  if (kind === "doc") return "DOC";
  return kind.toUpperCase().replace(/[_-]+/g, " ");
}

/** #26 — the lifecycle status of a durable mission (mission.rs MissionStatus,
 *  snake_case on the wire). Closed union; an unknown status is coerced to the
 *  SAFE "paused" by the parser — a junk status must NEVER read as "active"
 *  (which would imply the mission is running when we cannot confirm it). */
export type MissionStatus = "active" | "paused" | "done" | "cancelled";

/** Coerce an untrusted mission status, defaulting to the safe "paused". */
export function coerceMissionStatus(v: unknown): MissionStatus {
  return v === "active" || v === "paused" || v === "done" || v === "cancelled"
    ? v
    : "paused";
}

/** #26 — one DURABLE MISSION record, folded from mission.saved / mission.resumed
 *  / mission.cancelled. SECRET-FREE: id / goal / status / a bounded sub-task
 *  progress (done-of-total) — never a sub-task's raw input, never a secret. The
 *  goal is the natural-language goal the persona already speaks. */
export interface DurableMission {
  id: string;
  goal: string;
  status: MissionStatus;
  /** Sub-tasks completed, of the bounded total (Fury caps at <=6). Both clamped
   *  to >= 0; `total` is 0 when the daemon sent no breakdown. */
  done: number;
  total: number;
  ts: string; // envelope ts of the last event that touched this mission
}

/** A non-negative integer field, defaulting to 0 (never NaN/negative on the
 *  progress bar). */
function nonNegIntOr0(data: Record<string, unknown>, key: string): number {
  const v = num(data, key);
  if (v === null) return 0;
  const i = Math.trunc(v);
  return i < 0 ? 0 : i;
}

/** Parse a mission lifecycle event (mission.saved / .resumed / .cancelled) into
 *  a DurableMission, or null if it lacks a usable id. For mission.cancelled the
 *  status is forced to "cancelled" (the event name is authoritative). Otherwise
 *  the status is coerced from the payload (defaulting to the SAFE "paused" — a
 *  saved mission loads PAUSED, never auto-active). SECRET-FREE. Never throws. */
export function parseMissionEvent(
  event: string,
  data: Record<string, unknown>,
  ts: string,
): DurableMission | null {
  const id = str(data, "id");
  if (id === null || id.length === 0) return null;
  const status: MissionStatus =
    event === "mission.cancelled" ? "cancelled" : coerceMissionStatus(data["status"]);
  return {
    id,
    goal: (str(data, "goal") ?? "").slice(0, MISSION_GOAL_CAP),
    status,
    done: nonNegIntOr0(data, "done"),
    total: nonNegIntOr0(data, "total"),
    ts,
  };
}

/** A human label for a mission status (for the panel pill). */
export function missionStatusLabel(status: MissionStatus): string {
  return status.toUpperCase();
}

/** #27 — the lifecycle phase of a macro's last replay, folded from
 *  macro.replay_started / .replay_step / .replay_done. SECRET-FREE — only the
 *  phase + the recorded intent/utterance (NEVER a resolved credential). */
export type MacroReplayPhase = "idle" | "running" | "done";

/** #27 — the last replay step (macro.replay_step). `intent` is the recorded
 *  intent NAME; `utterance` is the recorded command the user originally spoke —
 *  the daemon stores INTENTS/UTTERANCES ONLY, never a secret/token/credential,
 *  so neither field can carry one. */
export interface MacroReplayStep {
  intent: string;
  utterance: string;
}

/** #27 — one recorded MACRO, folded from macro.recorded (+ replay lifecycle).
 *  SECRET-FREE: the name, the step COUNT, and the last replay phase/step — the
 *  daemon stores only intents/utterances and never streams a credential. The
 *  literal recorded commands are NOT broadcast here in bulk; only the last
 *  replayed step (intent + the spoken utterance) is surfaced as live progress. */
export interface MacroEntry {
  name: string;
  /** How many commands the macro recorded (macro.recorded `steps`). */
  steps: number;
  /** The last replay phase for this macro (drives the "replaying…/done" note). */
  replayPhase: MacroReplayPhase;
  /** The most recent replay step shown as live progress, or null when idle. */
  lastStep: MacroReplayStep | null;
  ts: string; // envelope ts of the last event that touched this macro
}

/** Parse a macro.recorded payload into a MacroEntry, or null if it lacks a
 *  usable name. `steps` is clamped to >= 0. SECRET-FREE: only the name + count.
 *  Never throws. */
export function parseMacroRecorded(
  data: Record<string, unknown>,
  ts: string,
): MacroEntry | null {
  const name = str(data, "name");
  if (name === null || name.length === 0) return null;
  return {
    name: name.slice(0, MACRO_NAME_CAP),
    steps: nonNegIntOr0(data, "steps"),
    replayPhase: "idle",
    lastStep: null,
    ts,
  };
}

/** Parse a macro.replay_step payload into a MacroReplayStep, or null if BOTH
 *  fields are missing (nothing to show). SECRET-FREE: the daemon stores only
 *  intents/utterances — neither can carry a credential. Never throws. */
export function parseMacroReplayStep(
  data: Record<string, unknown>,
): MacroReplayStep | null {
  const intent = str(data, "intent") ?? "";
  const utterance = str(data, "utterance") ?? "";
  if (intent.length === 0 && utterance.length === 0) return null;
  return { intent, utterance };
}

/* ------------------------------------------------------------------------ *
 * DATA -> CHART (#41; chart.rs ChartSpec / chart.data).                       *
 *                                                                            *
 * The daemon's chart.rs emits `chart.data` from a data path (a "chart this"  *
 * voice op serializes the latest REAL system snapshot into a ChartSpec, or    *
 * any data-producing op surfaces its series). ChartSpec::to_telemetry yields  *
 * the EXACT wire shape:                                                       *
 *   {"kind": "bar"|"line"|"sparkline", "title", "x_axis", "y_axis",           *
 *    "empty": bool, "series": [{"label", "points": [[x,y], ...]}]}            *
 *                                                                            *
 * The HUD's Chart component is the EXACT renderer: every emitted point is      *
 * plotted, line segments only connect the GIVEN points (NO interpolation, NO   *
 * resampling, NO invented/extrapolated point), the axis ranges are DERIVED     *
 * from the data, and an empty spec renders the honest-empty state. The op      *
 * ships OFF ([chart].enabled) so nothing arrives until it is deliberately      *
 * enabled — the chart is a NEUTRAL presentation surface (no gate, no action,   *
 * no network). SECRET-FREE: only the labels, axis strings, title, and the      *
 * numeric points ride the wire.                                                *
 * ------------------------------------------------------------------------ */

/** How the HUD draws between points (chart.rs ChartKind::as_str, lowercase on
 *  the wire). Closed union — an unknown kind is dropped by the parser so the
 *  renderer never sees an unrecognized mode. */
export type ChartKind = "bar" | "line" | "sparkline";

/** One plotted point — an [x, y] pair, EXACTLY as the daemon emitted it. No
 *  rounding, no resampling: the renderer plots these verbatim. */
export interface ChartPoint {
  x: number;
  y: number;
}

/** One series to plot: a label and its ordered points. A series with NO usable
 *  point is dropped by the parser (nothing to draw), never given a fabricated
 *  one. */
export interface ChartSeries {
  label: string;
  points: ChartPoint[];
}

/** A complete, defensively-parsed `chart.data` payload — the HUD's exact view of
 *  one chart to draw. `empty` is carried explicitly by the daemon (true when no
 *  series carries a point) AND re-derived defensively here, so a malformed wire
 *  `empty` can never make the renderer claim data it does not have. Every point
 *  is one the daemon emitted; the renderer never invents/interpolates/
 *  extrapolates. SECRET-FREE by construction. */
export interface ChartSpec {
  kind: ChartKind;
  title: string;
  xAxis: string;
  yAxis: string;
  series: ChartSeries[];
  /** True when there is NOTHING to plot (no series, or every series empty) — the
   *  renderer shows the honest-empty state. Re-derived from `series`, never
   *  trusted from the wire alone. */
  empty: boolean;
}

/** Cap on the number of series the HUD plots from one chart.data frame, and on
 *  the points per series — VIEW bounds so a misbehaving/oversized frame cannot
 *  grow the render without limit. The daemon's producers are themselves bounded;
 *  these are belt-and-suspenders. */
export const CHART_SERIES_CAP = 12;
export const CHART_POINTS_CAP = 512;

/** Coerce one untrusted point into a ChartPoint, or null when EITHER coordinate
 *  is missing or non-finite — a half-point is not plottable, so it is dropped
 *  (never zero-filled into a fabricated point). A wire point is `[x, y]` (a
 *  2-element array); anything else is dropped. Never throws. */
function coerceChartPoint(v: unknown): ChartPoint | null {
  if (!Array.isArray(v) || v.length < 2) return null;
  const x = v[0];
  const y = v[1];
  if (typeof x !== "number" || !Number.isFinite(x)) return null;
  if (typeof y !== "number" || !Number.isFinite(y)) return null;
  return { x, y };
}

/** Coerce one untrusted series object into a ChartSeries, or null when it has NO
 *  usable point — a series with nothing to draw is dropped rather than rendered
 *  as an empty stub. Points are coerced item-by-item (a malformed point is
 *  dropped, never zero-filled) and bounded by CHART_POINTS_CAP. `label` defaults
 *  to "" when absent. Never throws. */
function coerceChartSeries(o: Record<string, unknown>): ChartSeries | null {
  const rawPoints = o["points"];
  if (!Array.isArray(rawPoints)) return null;
  const points = rawPoints
    .map(coerceChartPoint)
    .filter((p): p is ChartPoint => p !== null)
    .slice(0, CHART_POINTS_CAP);
  if (points.length === 0) return null;
  return { label: str(o, "label") ?? "", points };
}

/** Parse a `chart.data` payload into a ChartSpec, or null when the `kind` is not
 *  one of the recognized modes (an unrecognized chart is dropped, never rendered
 *  with a guessed mode). Series are coerced item-by-item (a series with no usable
 *  point is dropped, never fabricated) and bounded by CHART_SERIES_CAP. `empty`
 *  is RE-DERIVED from the surviving series — never trusted from the wire — so a
 *  spoofed `empty: false` over an empty series still renders honest-empty, and a
 *  spoofed `empty: true` over real points still plots them. SECRET-FREE: only the
 *  kind, axis strings, title, labels, and numeric points are read. Never throws. */
export function parseChartSpec(data: Record<string, unknown>): ChartSpec | null {
  const kind = str(data, "kind");
  if (kind !== "bar" && kind !== "line" && kind !== "sparkline") return null;
  const rawSeries = data["series"];
  const series = Array.isArray(rawSeries)
    ? rawSeries
        .filter(isPlainObject)
        .map(coerceChartSeries)
        .filter((s): s is ChartSeries => s !== null)
        .slice(0, CHART_SERIES_CAP)
    : [];
  return {
    kind,
    title: str(data, "title") ?? "",
    xAxis: str(data, "x_axis") ?? "",
    yAxis: str(data, "y_axis") ?? "",
    series,
    // Honest-empty is re-derived from the actual surviving points, NOT trusted
    // from the wire — the renderer must never claim data it does not hold.
    empty: series.every((s) => s.points.length === 0),
  };
}

/* ------------------------------------------------------------------------ *
 * REPORT GENERATION (#40; report.rs Report / report.built).                   *
 *                                                                            *
 * The daemon's report.rs assembles already-cited notebook/research sources    *
 * into a structured report under the SAME citation discipline (a claim with   *
 * no usable citation is DROPPED, never given a fabricated one) and router.rs   *
 * emits `report.built`:                                                       *
 *   {"verb": "report"|"report_empty"|"report_off"|"error",                    *
 *    "report": {"title", "empty": bool, "section_count", "headings": [...],    *
 *               "citation_count", "citations": [{"id", "title", "url"}]} | null}*
 *                                                                            *
 * The HUD's report readout surfaces the report's title + section count +       *
 * citation count + a bounded preview of the real citations. EVERY citation is  *
 * a REAL source ref an input claim carried — the daemon never synthesizes one, *
 * and the parser drops any citation with no usable locator. An honest-empty    *
 * report (no citable source) surfaces the plain "no sources to report on", and *
 * the off/error verbs carry NO report so the panel shows nothing. The op ships *
 * OFF ([report].enabled). REVIEW-ONLY: there is no button — it SHOWS the report *
 * the daemon already built. SECRET-FREE: only the title, headings, counts, and *
 * the real citation locators ride the wire — never raw body content.           *
 * ------------------------------------------------------------------------ */

/** One CITATION the report rests on (report.rs ReportCitation): the run-local id,
 *  the title, and the real URL — exactly the locator an input claim carried.
 *  NEVER fabricated. */
export interface ReportCitation {
  id: number;
  title: string;
  url: string;
}

/** A defensively-parsed `report.built` report object — the HUD's honest view of
 *  one built report. `empty` is the honest "nothing citable to report on" flag,
 *  RE-DERIVED defensively (no real citations AND no sections => empty) so a
 *  spoofed wire flag can never claim content the citations do not back. Every
 *  citation is a REAL source ref an input claim carried. SECRET-FREE: counts +
 *  headings + the real locators only, never raw body content. */
export interface ReportReadout {
  title: string;
  empty: boolean;
  sectionCount: number;
  headings: string[];
  citationCount: number;
  citations: ReportCitation[];
}

/** Cap on the headings + citations the HUD previews from one report.built frame —
 *  VIEW bounds (the report readout is a PREVIEW, not the full document; the
 *  rendered markdown is spoken/shown elsewhere). The daemon's builder is itself
 *  bounded (MAX_SECTIONS / MAX_CITATIONS); these keep the preview tidy. */
export const REPORT_HEADINGS_CAP = 12;
export const REPORT_CITATIONS_CAP = 16;

/** Coerce one untrusted citation object into a ReportCitation, or null when it
 *  lacks a usable locator — BOTH `title` and `url` blank means there is nothing
 *  to point at, so it is dropped (never fabricated). A non-finite/absent `id`
 *  defaults to 0. Never throws. */
function coerceReportCitation(o: Record<string, unknown>): ReportCitation | null {
  const title = (str(o, "title") ?? "").trim();
  const url = (str(o, "url") ?? "").trim();
  if (title.length === 0 && url.length === 0) return null;
  return { id: nonNegIntOr0(o, "id"), title, url };
}

/** Parse a `report.built` payload into a ReportReadout, or null when there is no
 *  `report` object (the off/error verbs carry `report: null`, so the panel shows
 *  nothing). Headings are coerced to non-empty strings + bounded; citations are
 *  coerced item-by-item (a citation with no usable locator is dropped, never
 *  fabricated) + bounded. The counts are taken as the daemon's totals (the
 *  builder's real section/citation counts), so a preview that is bounded shorter
 *  than the total still reports the honest total. `empty` is RE-DERIVED — a
 *  report with no surviving citation AND no section is honest-empty regardless of
 *  the wire flag. SECRET-FREE. Never throws. */
export function parseReportReadout(data: Record<string, unknown>): ReportReadout | null {
  const report = isPlainObject(data["report"]) ? data["report"] : null;
  if (report === null) return null;
  const headings = (strArr(report, "headings") ?? [])
    .map((h) => h.trim())
    .filter((h) => h.length > 0)
    .slice(0, REPORT_HEADINGS_CAP);
  const rawCitations = report["citations"];
  const citations = Array.isArray(rawCitations)
    ? rawCitations
        .filter(isPlainObject)
        .map(coerceReportCitation)
        .filter((c): c is ReportCitation => c !== null)
        .slice(0, REPORT_CITATIONS_CAP)
    : [];
  const sectionCount = nonNegIntOr0(report, "section_count");
  const citationCount = nonNegIntOr0(report, "citation_count");
  return {
    title: (str(report, "title") ?? "").trim(),
    // Honest-empty re-derived: no real citations AND no sections => nothing was
    // citable to report on, regardless of the wire `empty`.
    empty: citations.length === 0 && sectionCount === 0,
    sectionCount,
    headings,
    citationCount,
    citations,
  };
}
