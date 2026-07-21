import { useCallback, useEffect, useRef, useState } from "react";
import Frame from "./Frame";
import useModalFocus from "./useModalFocus";
import SystemSettingsPanel from "./SystemSettingsPanel";
import SystemAccessPanel from "./SystemAccessPanel";
import {
  BEARER_CREDENTIALS,
  Credential,
  OAUTH_CREDENTIALS,
  PillState,
  hintForId,
  mcpTokenAccount,
  pillClass,
  pillFromPresence,
  pillFromVerify,
  pillLabel,
} from "../core/credentials";
import type {
  DocIndexStatus,
  LockdownStatus,
  McpStatus,
  ModelSwapIntent,
  ModelTierStatus,
  PolicyDecision,
  PolicyRule,
  PolicySnapshot,
  SecurityStatus,
  SttTierStatus,
  VoiceIdStatus,
} from "../core/events";
import {
  PANIC_CONFIRMATION,
  UNLOCK_CONFIRMATION,
  lockdownLabel,
  lockdownTone,
  securityLabel,
  securityTone,
  modelTierHonest,
  modelTierLabel,
  modelTierModeLabel,
  modelTierReasonHonest,
  modelTierReasonLabel,
  modelTierTone,
  sttTierDetail,
  sttTierLabel,
  sttTierTone,
  voiceIdDisplay,
  voiceIdLabel,
  voiceIdSimilarityPct,
  voiceIdTone,
} from "../core/events";
import {
  beginAuthForId,
  inTauri,
  keychainDelete,
  keychainSet,
  keychainStatus,
  verifyAndStore,
} from "../tauri/bridge";
import { sendCommand } from "../tauri/command";

/**
 * The EXACT spoken model-control phrases the four "Set tier" buttons send over
 * the command channel as `{cmd:"ask", text}`. These are interpreted by the
 * daemon's CONSERVATIVE `classify_model_swap` (daemon/src/model_tier.rs) — each
 * MUST match an anchor there or the click leaks to the normal answer path and
 * the override is never set/cleared. The AUTO phrase in particular must classify
 * (clearing the override), which `"auto, you pick the model"` did NOT — it is
 * `"auto mode"` here on purpose. The daemon test
 * `settings_button_phrases_round_trip_to_their_intent` locks these literals to
 * the classifier; the HUD test locks the buttons to these literals. Both ends of
 * the round-trip are covered, so a phrase edit on either side fails CI.
 */
export const MODEL_SWAP_BUTTON_PHRASES: Record<ModelSwapIntent, string> = {
  heavy: "use the most powerful model",
  fast: "use the fast model",
  local: "work offline, stay on device",
  auto: "auto mode",
};

/**
 * The EXACT spoken phrases the voice-clone control sends over the command channel
 * as `{cmd:"ask", text}` — interpreted by the daemon's CONSENT-GATED clone machine
 * (daemon/src/voiceclone.rs). They are anchored to that classifier so a phrase edit
 * on either side fails CI:
 *   - `propose`  -> `voiceclone::classify_intent` must return `CloneIntent::Clone`,
 *     which PARKS `CloneState::Pending` and SPEAKS the honest consent prompt. NOTHING
 *     leaves the device on this turn — the daemon is now AWAITING a spoken yes.
 *   - `confirm`  -> on the NEXT turn `voiceclone::is_confirmation` must read this as a
 *     clear YES, so the parked clone proceeds (the sample is uploaded). The HUD never
 *     uploads itself and adds NO new authority — it asks the daemon, exactly like the
 *     voice path, and the two-step (propose, then a SEPARATE explicit confirm) mirrors
 *     the daemon's own cross-turn consent gate so a single click can never upload.
 *   - `forget`  -> `voiceclone::classify_intent` must return `CloneIntent::Forget`,
 *     which drops the stored clone slot (back to Kokoro / the existing voice).
 */
export const VOICE_CLONE_PHRASES = {
  propose: "clone my voice with ElevenLabs",
  confirm: "yes, go ahead and clone my voice",
  forget: "forget my voice clone",
} as const;

/**
 * The EXACT phrases the POLICY editor sends over the command channel as the
 * DEDICATED `{cmd:"policy", text}` verb (NOT `ask` — these never reach the model
 * tool loop). These are USER-SET-ONLY writes: the daemon's policy verb classifier
 * (`policy::classify_policy_command` in daemon/src/policy.rs, reached from the
 * command channel's `policy` dispatcher arm and the post-voice-id router) parses
 * them and applies them via the user-only write path (`policy::apply_global`).
 * There is NO agent/model/tool write path; only this explicit user action (a
 * click here, or speaking the same phrase after voice-id) can change a rule, which
 * preserves the invariant that an injected "set policy allow X" reaching the model
 * is impossible — the model has no policy-write tool, and the `policy` verb is not
 * routed through `complete_with_tools`.
 *
 * HONESTY pinned at the call site:
 *   - `always(tool)` -> a deliberate, MASTER-GATED loosening. It auto-approves
 *     ONLY when the master switch is ON and voice-id allows (enforced daemon-side);
 *     it is INERT when the master switch is OFF. It NEVER overrides the master.
 *   - `never(tool)`  -> a hard block that ALWAYS wins (even master ON + a fresh
 *     confirmation).
 *   - `ask(tool)`    -> clears any rule back to the default park/confirm (ASK).
 *
 * Each builder names the verb + the tool explicitly so the spoken text is
 * unambiguous and audit-legible. The tool name is the user's chosen consequential
 * tool (e.g. "gmail_send"); the daemon anchors the verb, never a blanket all-tools
 * rule. Like the other Settings phrase tables, a daemon-side round-trip test locks
 * these literals to the classifier so a phrase edit on either side fails CI.
 */
export const POLICY_PHRASES = {
  always: (tool: string) => `always allow the ${tool} action`,
  never: (tool: string) => `never allow the ${tool} action`,
  ask: (tool: string) => `always ask before the ${tool} action`,
} as const;

/**
 * Settings panel — multi-credential registry (docs/HUD.md §5.1, CONTRACT
 * part C). Each bearer credential is stored as a macOS Keychain generic
 * password (service com.darwin.daemon, account from the registry), never
 * plaintext, never logged. The panel renders presence only — secrets are never
 * rendered back. Pressing ENTER in a row verifies the token live and, only on
 * success, stores it.
 */
export default function SettingsModal({
  mcp,
  voiceId,
  modelTier,
  sttTier,
  docIndex = null,
  policy = null,
  security = null,
  lockdown = null,
  onLockedChange,
  initialTab = "credentials",
  onReopenOnboarding,
  onClose,
}: {
  mcp: McpStatus | null;
  voiceId: VoiceIdStatus;
  modelTier: ModelTierStatus;
  sttTier: SttTierStatus;
  /** The live on-device file-index status (docsearch.indexed). Optional so the
   *  existing settings tests can render the modal without it; defaults to null
   *  (the honest "not indexed yet" state). */
  docIndex?: DocIndexStatus | null;
  /** The user-set policy snapshot (policy.snapshot) for the per-action policy
   *  editor. Optional so the existing settings tests can render the modal without
   *  it; defaults to null (the honest "ASK everywhere / awaiting" state). */
  policy?: PolicySnapshot | null;
  /** The live at-rest-encryption posture (security.status) for the encryption
   *  status indicator + the enable documentation. Optional so the existing
   *  settings tests can render the modal without it; defaults to null (the honest
   *  "awaiting" state before the daemon emits the startup snapshot). */
  security?: SecurityStatus | null;
  /** The live PANIC / LOCKDOWN posture (lockdown.status) for the emergency-stop
   *  indicator + the PANIC / UNLOCK controls. Optional so the existing settings
   *  tests render without it; defaults to null (the honest "awaiting" state before
   *  the daemon emits the startup snapshot). */
  lockdown?: LockdownStatus | null;
  /** Fold a panic/unlock COMMAND REPLY's `locked` verdict into the HUD indicator
   *  immediately (App dispatches lockdown.set). Optional so the existing tests
   *  render without it; when absent the section still works (the indicator just
   *  waits for the next telemetry frame instead of flipping instantly). */
  onLockedChange?: (locked: boolean) => void;
  /** Which tab to open on. Defaults to "credentials" (today's behavior). The
   *  onboarding wizard routes to a specific tab by passing this — it never adds
   *  a new surface, it just deep-opens an existing one. */
  initialTab?: "credentials" | "system" | "access";
  /** Re-open the first-run onboarding tour (a deliberate user action). Optional
   *  so the existing tests render without it; when absent the control is hidden. */
  onReopenOnboarding?: () => void;
  onClose: () => void;
}) {
  const shell = inTauri();
  // Which top-level Settings surface is showing. "credentials" is the existing
  // keys/gates/policy view; "system" is the dedicated SYSTEM SETTINGS panel that
  // edits config/darwin.toml (batched, applied on a daemon restart).
  const [tab, setTab] = useState<"credentials" | "system" | "access">(initialTab);
  // The configured MCP servers that declare a token (mcp.status carries only the
  // usesToken bool — never a secret), so a server's token can be stored under its
  // mcp_<server>_token Keychain account through the SAME guarded path.
  const mcpTokenServers = (mcp?.servers ?? []).filter((s) => s.usesToken);

  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  // a11y: trap + autofocus + focus-restore (Escape stays on the window
  // listener above — see useModalFocus's double-fire note).
  const modalRef = useRef<HTMLDivElement>(null);
  useModalFocus(modalRef);

  return (
    <div className="modal-backdrop" onClick={onClose}>
      {/* a11y: real dialog semantics + a real focus trap (autofocus, Tab
          cycle, focus restore) via useModalFocus. Escape is NOT wired here —
          the window-level listener above already closes; both would
          double-fire. */}
      <div
        className="modal settings-modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-modal="true"
        aria-label="Settings"
        ref={modalRef}
      >
        <Frame
          title={
            tab === "system"
              ? "SETTINGS // SYSTEM CONFIG"
              : tab === "access"
                ? "SETTINGS // SYSTEM ACCESS"
                : "SETTINGS // CREDENTIALS"
          }
          tag="com.darwin.daemon"
        >
          <div className="settings-tabs" role="tablist" aria-label="Settings sections">
            <button
              type="button"
              role="tab"
              aria-selected={tab === "credentials"}
              className={`settings-tab${tab === "credentials" ? " active" : ""}`}
              onClick={() => setTab("credentials")}
            >
              Credentials &amp; Gates
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={tab === "system"}
              className={`settings-tab${tab === "system" ? " active" : ""}`}
              onClick={() => setTab("system")}
            >
              System Settings
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={tab === "access"}
              className={`settings-tab${tab === "access" ? " active" : ""}`}
              onClick={() => setTab("access")}
            >
              System Access
            </button>
          </div>

          {tab === "system" ? (
            <div className="body" role="tabpanel" aria-label="System Settings">
              <SystemSettingsPanel voiceId={voiceId} />
              <div className="field-row" style={{ justifyContent: "flex-end" }}>
                <button className="icon-btn" onClick={onClose}>
                  Close
                </button>
              </div>
            </div>
          ) : tab === "access" ? (
            <div className="body" role="tabpanel" aria-label="System Access">
              <SystemAccessPanel />
              <div className="field-row" style={{ justifyContent: "flex-end" }}>
                <button className="icon-btn" onClick={onClose}>
                  Close
                </button>
              </div>
            </div>
          ) : (
          <div className="body" role="tabpanel" aria-label="Credentials &amp; Gates">
            <div className="cred-section-title">
              PANIC // EMERGENCY STOP (LOCKDOWN)
            </div>
            <LockdownSection
              lockdown={lockdown}
              shell={shell}
              onLockedChange={onLockedChange}
            />

            <div className="cred-section-title">CREDENTIALS</div>
            {BEARER_CREDENTIALS.map((cred) => (
              <BearerRow key={cred.id} cred={cred} shell={shell} />
            ))}

            <div className="cred-section-title">INTEGRATIONS (OAUTH)</div>
            {OAUTH_CREDENTIALS.map((cred) => (
              <OAuthConnectRow key={cred.id} cred={cred} shell={shell} />
            ))}

            {mcpTokenServers.length > 0 && (
              <>
                <div className="cred-section-title">MCP SERVER TOKENS</div>
                {mcpTokenServers.map((s) => (
                  <McpTokenRow key={s.name} server={s.name} shell={shell} />
                ))}
                <div className="kv-note">
                  These are the tokens for the MCP servers configured in
                  darwin.toml that declare one. The token is stored under the
                  server&apos;s own Keychain account and is never shown, logged,
                  on argv, or in a URL. There is no live verify for an MCP token
                  (it is server-specific), so storing it saves it directly. To
                  add or remove a server itself, edit darwin.toml and restart
                  darwind — the HUD does not spawn servers.
                </div>
              </>
            )}

            <div className="cred-section-title">
              CONSEQUENTIAL POLICY // PER-ACTION ALLOW / NEVER / ASK
            </div>
            <PolicySection policy={policy} shell={shell} />

            <div className="cred-section-title">VOICE-ID // SPEAKER MATCH</div>
            <VoiceIdSection voiceId={voiceId} />

            <div className="cred-section-title">VOICE CLONE // YOUR OWN VOICE (CONSENT-GATED)</div>
            <VoiceCloneSection shell={shell} />

            <div className="cred-section-title">CLOUD STT // TRANSCRIPTION</div>
            <SttTierSection sttTier={sttTier} />

            <div className="cred-section-title">MODEL TIER // WHICH MODEL ANSWERS</div>
            <ModelTierSection modelTier={modelTier} shell={shell} />

            <div className="cred-section-title">MEMORY // EPISODES + USER MODEL</div>
            <MemorySection />

            <div className="cred-section-title">
              ENCRYPTION // AT-REST ON DISK (SQLCIPHER, OPT-IN)
            </div>
            <EncryptionSection security={security} />

            <div className="cred-section-title">FILE SEARCH // ON-DEVICE (TEXT-LIKE, v1)</div>
            <DocSearchSection docIndex={docIndex} />

            <div className="kv-note">
              Storing a token verifies and saves it; the agent that uses it
              (e.g. GitHub PRs) is a separate build. Restart darwind after
              changing the Anthropic key. Google Workspace, X, and LinkedIn each
              need your own developer OAuth app (client id + secret) plus a
              one-time browser consent that runs in darwind — they are not
              paste-only like GitHub or Slack.
            </div>

            <div className="field-row" style={{ justifyContent: "space-between" }}>
              {onReopenOnboarding ? (
                <button
                  className="icon-btn"
                  onClick={onReopenOnboarding}
                  title="Reopen the first-run welcome tour"
                >
                  Reopen welcome tour
                </button>
              ) : (
                <span />
              )}
              <button className="icon-btn" onClick={onClose}>
                Close
              </button>
            </div>
          </div>
          )}
        </Frame>
      </div>
    </div>
  );
}

/** One bearer credential row: label, masked input (Enter = verify+store), pill,
 *  and a Remove (X) affordance. */
function BearerRow({ cred, shell }: { cred: Credential; shell: boolean }) {
  const [value, setValue] = useState("");
  const [pill, setPill] = useState<PillState>({ kind: "empty" });
  const [busy, setBusy] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  // On open, reflect ON FILE vs empty from the Keychain.
  const refresh = useCallback(() => {
    keychainStatus(cred.keychain_account)
      .then((onFile) => setPill(pillFromPresence(onFile)))
      .catch(() => setPill({ kind: "empty" }));
  }, [cred.keychain_account]);

  useEffect(refresh, [refresh]);

  // ENTER -> verify + store. On valid+stored the input clears and the pill
  // shows ON FILE; otherwise the pill reflects the failure. The secret is
  // never logged and never rendered back.
  const submit = useCallback(async () => {
    const secret = value.trim();
    if (!secret || busy) return;
    setBusy(true);
    setPill({ kind: "verifying" });
    try {
      const result = await verifyAndStore(cred.id, secret);
      const next = pillFromVerify(result);
      setPill(next);
      if (next.kind === "on_file") setValue(""); // never keep the secret in state
    } catch {
      // The error string from the shell could carry context but never the
      // secret; surface a generic network state without console logging.
      setPill({ kind: "network", detail: "shell error" });
    } finally {
      setBusy(false);
    }
  }, [value, busy, cred.id]);

  const remove = useCallback(async () => {
    setBusy(true);
    try {
      await keychainDelete(cred.keychain_account);
      setValue("");
      setPill({ kind: "empty" });
    } catch {
      // leave the pill; deletion failure is non-fatal to the panel
    } finally {
      setBusy(false);
    }
  }, [cred.keychain_account]);

  const detail =
    pill.kind === "valid" ||
    pill.kind === "invalid" ||
    pill.kind === "network"
      ? pill.detail
      : "";

  const hint = hintForId(cred.id);

  return (
    <div className="cred-row">
      <div className="cred-label">{cred.label}</div>
      <input
        ref={inputRef}
        className="cred-input"
        type="password"
        autoComplete="off"
        autoCorrect="off"
        autoCapitalize="off"
        spellCheck={false}
        placeholder={cred.id === "anthropic" ? "sk-ant-…" : "paste token, press Enter"}
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            void submit();
          }
        }}
        disabled={!shell || busy}
      />
      <button
        className="icon-btn cred-remove"
        onClick={() => void remove()}
        disabled={!shell || busy || pill.kind !== "on_file"}
        title="remove from Keychain"
        aria-label={`remove ${cred.label}`}
      >
        ✕
      </button>
      {hint ? <div className="cred-hint">{hint}</div> : null}
      <div className={`cred-pill ${pillClass(pill)}`}>
        {pillLabel(pill)}
        {detail ? ` — ${detail}` : ""}
      </div>
    </div>
  );
}

/** An OAuth connection-STATUS row (Google / X / LinkedIn). There is nothing to
 *  paste here: the refresh token is minted by the daemon's browser consent flow.
 *  We show CONNECTED only when that token is genuinely on file; otherwise a
 *  Connect affordance that triggers the daemon's flow at runtime — the HUD
 *  itself never opens a browser or fakes a connection. The connect verb is
 *  derived from the credential label (the platform's display name). */
function OAuthConnectRow({
  cred,
  shell,
}: {
  cred: Credential;
  shell: boolean;
}) {
  // Presence of the refresh token == genuinely connected.
  const [connected, setConnected] = useState(false);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");

  const refresh = useCallback(() => {
    keychainStatus(cred.keychain_account)
      .then(setConnected)
      .catch(() => setConnected(false));
  }, [cred.keychain_account]);

  useEffect(refresh, [refresh]);

  const connect = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    try {
      const r = await beginAuthForId(cred.id)();
      // Never claim connected unless the shell confirms a refresh token on file.
      setConnected(r.connected);
      setNote(r.detail);
    } catch {
      setNote("shell error");
    } finally {
      setBusy(false);
    }
  }, [busy, cred.id]);

  // Honest pill: CONNECTED only when a refresh token exists.
  const pill: PillState = connected
    ? { kind: "on_file" }
    : { kind: "empty" };
  const pillText = connected ? "CONNECTED" : "NOT CONNECTED";
  // The platform's display name drives the button verb (e.g. "CONNECT X").
  const verb = cred.label.toUpperCase();

  return (
    <div className="cred-row">
      <div className="cred-label">{cred.label}</div>
      <button
        className="icon-btn"
        onClick={() => void connect()}
        disabled={!shell || busy}
        title="runs the daemon's browser consent at runtime"
      >
        {connected ? `RECONNECT ${verb}` : `CONNECT ${verb}`}
      </button>
      <div className="cred-hint">
        {note ||
          "Paste the client id + secret above, then Connect. The browser consent runs in darwind."}
      </div>
      <div className={`cred-pill ${pillClass(pill)}`}>{pillText}</div>
    </div>
  );
}

/** One MCP server token row: paste the server's token, Enter to STORE it under
 *  `mcp_<server>_token` (no live verify — an MCP token has no paste-time check),
 *  presence pill, and a Remove affordance. The account is computed via
 *  `mcpTokenAccount`, which returns null for a non-allowlisted server name — so a
 *  hostile name yields no write. The secret is never logged or echoed back. */
function McpTokenRow({ server, shell }: { server: string; shell: boolean }) {
  const account = mcpTokenAccount(server);
  const [value, setValue] = useState("");
  const [pill, setPill] = useState<PillState>({ kind: "empty" });
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(() => {
    if (account === null) return;
    keychainStatus(account)
      .then((onFile) => setPill(pillFromPresence(onFile)))
      .catch(() => setPill({ kind: "empty" }));
  }, [account]);

  useEffect(refresh, [refresh]);

  // ENTER -> store directly (no verify). On success the input clears and the
  // pill shows ON FILE; the secret is never kept in state past the write.
  const submit = useCallback(async () => {
    const secret = value.trim();
    if (!secret || busy || account === null) return;
    setBusy(true);
    try {
      await keychainSet(account, secret);
      setValue(""); // never keep the secret in state
      setPill({ kind: "on_file" });
    } catch {
      // The error never carries the secret; surface a generic state.
      setPill({ kind: "network", detail: "shell error" });
    } finally {
      setBusy(false);
    }
  }, [value, busy, account]);

  const remove = useCallback(async () => {
    if (account === null) return;
    setBusy(true);
    try {
      await keychainDelete(account);
      setValue("");
      setPill({ kind: "empty" });
    } catch {
      // deletion failure is non-fatal to the panel
    } finally {
      setBusy(false);
    }
  }, [account]);

  // A non-allowlisted server name yields no account -> nothing storable.
  const disabled = !shell || busy || account === null;
  const detail = pill.kind === "network" ? pill.detail : "";

  return (
    <div className="cred-row">
      <div className="cred-label">{server}</div>
      <input
        className="cred-input"
        type="password"
        autoComplete="off"
        autoCorrect="off"
        autoCapitalize="off"
        spellCheck={false}
        placeholder="paste token, press Enter"
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            void submit();
          }
        }}
        disabled={disabled}
      />
      <button
        className="icon-btn cred-remove"
        onClick={() => void remove()}
        disabled={!shell || busy || pill.kind !== "on_file"}
        title="remove from Keychain"
        aria-label={`remove ${server} token`}
      >
        ✕
      </button>
      <div className="cred-hint">
        {account === null
          ? "invalid server name — nothing to store"
          : `Keychain account ${account}`}
      </div>
      <div className={`cred-pill ${pillClass(pill)}`}>
        {pillLabel(pill)}
        {detail ? ` — ${detail}` : ""}
      </div>
    </div>
  );
}

/** VOICE-ID // SPEAKER MATCH — the review + control surface for on-device
 *  speaker verification (daemon/src/voiceid.rs). HONESTY CONTRACT (do not
 *  regress): this is a LIGHTWEIGHT acoustic match (filterbank statistics +
 *  cosine), NOT a deep neural speaker-verification net and NOT a high-assurance
 *  biometric. It RAISES the bar — an obviously different voice is rejected — but
 *  is spoofable by a recording or a good impression. The hard backstop for
 *  outward actions remains the OFF-by-default consequential gate + master switch;
 *  voice-id is an ADDED layer, never a replacement. The score is a SIMILARITY in
 *  [0,1], never a probability of identity and never a security guarantee.
 *
 *  Like MCP servers and the [skills] master switch, voice-id is configured in
 *  darwin.toml (the HUD never writes daemon config), and enrolment is by an
 *  EXPLICIT spoken intent ("enroll my voice") — never automatic. So this section
 *  REFLECTS the live state from telemetry and documents the exact lockstep keys
 *  + the spoken controls, rather than offering a toggle that would silently do
 *  nothing. The key names below are byte-for-byte the daemon's [voice_id] keys
 *  (config.rs KNOWN_KEYS: enabled / threshold / min_enroll_samples / gate_scope). */
function VoiceIdSection({ voiceId }: { voiceId: VoiceIdStatus }) {
  const display = voiceIdDisplay(voiceId);
  const tone = voiceIdTone(display);
  const label = voiceIdLabel(display);
  const sim = voiceIdSimilarityPct(voiceId);

  // The live status line under the section title — the same honest verdict the
  // StatusBar chip shows, expanded with a similarity readout when there is one.
  const statusDetail =
    display === "enrolling"
      ? voiceId.need !== null
        ? `capturing ${voiceId.captured ?? 0}/${(voiceId.captured ?? 0) + voiceId.need}`
        : "capturing samples"
      : (display === "verified" || display === "unrecognized") && sim !== null
        ? `similarity ${sim}%`
        : "";

  return (
    <>
      <div className="cred-row">
        <div className="cred-label">Speaker match</div>
        <div className="cred-hint">
          On-device voice match. A lightweight acoustic check that raises the bar
          — NOT a biometric, and spoofable by a recording or a good impression.
          It is an ADDED layer on top of the confirmation gate, never a
          replacement for it. The score is a SIMILARITY, not a guarantee.
        </div>
        <div className={`cred-pill ${tone}`}>
          {label}
          {statusDetail ? ` — ${statusDetail}` : ""}
        </div>
      </div>

      <div className="kv-note">
        Voice-id is configured in <code>darwin.toml</code> under{" "}
        <code>[voice_id]</code> (the HUD does not write daemon config) and ships
        OFF — with it off, or with no enrolled profile, behavior is unchanged.
        Keys:
        <ul className="voiceid-keys">
          <li>
            <code>enabled</code> — master switch (default <code>false</code>).
            When off, voice-id enforces nothing.
          </li>
          <li>
            <code>threshold</code> — cosine SIMILARITY (0–1, default{" "}
            <code>0.86</code>) a voice must reach to count as you. Higher =
            stricter (fewer false accepts, more false rejects). This is a
            similarity cut-off, NOT a measured accuracy.
          </li>
          <li>
            <code>min_enroll_samples</code> — how many short phrases enrolment
            captures (default <code>3</code>).
          </li>
          <li>
            <code>gate_scope</code> — <code>&quot;consequential&quot;</code>{" "}
            (default: gate only outward/consequential actions + confirmation
            replay) or <code>&quot;all&quot;</code> (also gate ordinary
            commands). Restart darwind after editing.
          </li>
        </ul>
        Enrolment is an EXPLICIT spoken intent, never automatic: say{" "}
        <b>&quot;enroll my voice&quot;</b> and repeat the prompted phrases; say{" "}
        <b>&quot;forget my voice&quot;</b> to clear the profile. Raw audio is
        never stored or uploaded — the profile is a local feature vector only.
        When enabled + enrolled, an unrecognized speaker cannot trigger an
        outward action or approve a parked confirmation; the master consequential
        switch + the confirmation gate still apply independently.
      </div>
    </>
  );
}

/** VOICE CLONE // YOUR OWN VOICE — the CONSENT-GATED control to register the
 *  owner's voice with ElevenLabs (daemon/src/voiceclone.rs). HONESTY CONTRACT (do
 *  not regress): cloning UPLOADS an audio SAMPLE — it LEAVES this device for
 *  ElevenLabs. It is therefore CONSENT-GATED + AUTHORIZATION-BOUND: never automatic,
 *  only on a sample confined to the DARWIN root (your own voice — no impersonating
 *  others), and it takes TWO explicit steps. Step 1 (PROPOSE) sends the spoken
 *  "clone my voice" intent; the daemon PARKS a pending consent and asks you to
 *  confirm — NOTHING leaves the device yet. Step 2 (CONFIRM) is a SEPARATE explicit
 *  click that sends a clear "yes"; only then is the sample uploaded. Anything other
 *  than confirming cancels (audio never leaves). With no ElevenLabs key the daemon
 *  uploads nothing and you keep your on-device Kokoro/existing voice. FORGET drops
 *  the stored clone. The HUD adds NO new authority — it sends the SAME spoken
 *  phrases the voice path uses; the consent machine lives in the daemon. Live clone
 *  quality is device/credential-gated and is NEVER claimed measured here. */
function VoiceCloneSection({ shell }: { shell: boolean }) {
  // `proposed` is a HUD-local two-step latch: the first click proposes (parks the
  // daemon's pending consent + reveals the explicit CONFIRM affordance); only the
  // SEPARATE confirm click sends the yes that lets the sample leave the device. A
  // single click can never upload — this mirrors the daemon's own cross-turn gate.
  const [proposed, setProposed] = useState(false);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");

  const send = useCallback(async (phrase: string): Promise<string> => {
    const r = await sendCommand({ cmd: "ask", text: phrase });
    return r.ok ? r.reply || "" : r.error || "command failed";
  }, []);

  // Step 1 — PROPOSE. Sends the spoken clone intent; the daemon parks the pending
  // consent and speaks the honest prompt. Nothing has left the device. We then
  // reveal the explicit CONFIRM control (the user must take a SECOND action).
  const propose = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    setNote("");
    try {
      const reply = await send(VOICE_CLONE_PHRASES.propose);
      setProposed(true);
      setNote(
        reply ||
          "DARWIN is asking you to confirm. Nothing has left the device yet — press CONFIRM CLONE to upload the sample, or CANCEL.",
      );
    } catch {
      setNote("shell error");
    } finally {
      setBusy(false);
    }
  }, [busy, send]);

  // Step 2 — CONFIRM. The SEPARATE explicit yes that authorizes the upload. Only
  // reachable after a propose, so the sample can never leave on a single click.
  const confirm = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    try {
      const reply = await send(VOICE_CLONE_PHRASES.confirm);
      setProposed(false);
      setNote(reply || "Clone requested.");
    } catch {
      setNote("shell error");
    } finally {
      setBusy(false);
    }
  }, [busy, send]);

  // CANCEL — clears the HUD's pending step WITHOUT sending a yes (so the daemon's
  // pending consent lapses unconfirmed; the audio never leaves). Local-only.
  const cancel = useCallback(() => {
    setProposed(false);
    setNote("Cancelled — nothing left the device.");
  }, []);

  // FORGET — drops the stored clone slot (back to Kokoro / the existing voice).
  const forget = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    setProposed(false);
    try {
      const reply = await send(VOICE_CLONE_PHRASES.forget);
      setNote(reply || "Forgot the voice clone.");
    } catch {
      setNote("shell error");
    } finally {
      setBusy(false);
    }
  }, [busy, send]);

  return (
    <>
      <div className="cred-row">
        <div className="cred-label">Clone my voice</div>
        <div className="cred-hint">
          Registers YOUR voice as an ElevenLabs cloud voice. HONEST: this UPLOADS an
          audio SAMPLE — it LEAVES this device for ElevenLabs. Only clone a voice you
          are authorized to use (your own — no impersonating others). It takes two
          explicit steps and nothing is uploaded until you confirm. With no
          ElevenLabs key nothing is uploaded and you keep your on-device voice.
        </div>
        <div className={`cred-pill ${proposed ? "warn" : "idle"}`}>
          {proposed ? "AWAITING CONFIRM" : "CONSENT-GATED"}
        </div>
      </div>

      <div className="cred-row">
        <div className="cred-label">{proposed ? "Confirm" : "Action"}</div>
        <div className="modeltier-controls">
          {!proposed ? (
            <button
              className="icon-btn"
              onClick={() => void propose()}
              disabled={!shell || busy}
              title="Step 1: propose a clone — the daemon parks a consent prompt; NOTHING leaves the device yet"
            >
              CLONE MY VOICE
            </button>
          ) : (
            <>
              <button
                className="icon-btn"
                onClick={() => void confirm()}
                disabled={!shell || busy}
                title="Step 2: confirm — this UPLOADS the audio sample to ElevenLabs (it leaves the device)"
              >
                CONFIRM CLONE (UPLOADS SAMPLE)
              </button>
              <button
                className="icon-btn"
                onClick={cancel}
                disabled={busy}
                title="Cancel — nothing leaves the device"
              >
                CANCEL
              </button>
            </>
          )}
          <button
            className="icon-btn cred-remove"
            onClick={() => void forget()}
            disabled={!shell || busy}
            title="Forget the stored voice clone — back to Kokoro / your existing voice"
          >
            FORGET CLONE
          </button>
        </div>
        <div className="cred-hint">
          {note ||
            "Step 1 proposes (the daemon asks you to confirm; nothing leaves the device). Step 2 is a SEPARATE confirm that uploads the sample. Sends the same spoken phrases the voice path uses."}
        </div>
      </div>

      <div className="kv-note">
        Cloning is CONSENT-GATED + AUTHORIZATION-BOUND, never automatic. The owner
        sample is chosen by the daemon from a CONFINED in-tree location (your
        voice-id enrolment audio under <code>state/voiceid/</code>, else a
        <code>state/voice-samples/</code> wav) — a path that escapes the DARWIN root
        is rejected, so a clone can never be pointed at someone else&apos;s
        recording. You can also say <b>&quot;clone my voice&quot;</b> by voice for the
        same two-step flow, and <b>&quot;forget my voice clone&quot;</b> to drop it.
        The audio SAMPLE leaves the device for ElevenLabs <i>only</i> after you
        confirm; with no key nothing is uploaded and your on-device voice stays. The
        ElevenLabs key is Keychain-only and is never shown, logged, or sent here.
        Live clone quality is device/credential-gated — it is not measured in this
        panel. ElevenLabs is a VOICE layer only — DARWIN keeps its own brain.
      </div>
    </>
  );
}

/** CLOUD STT // TRANSCRIPTION — the review surface for the gated cloud-STT tier
 *  (daemon/src/voice_tier.rs::resolve_stt_backend + speech.rs). HONESTY CONTRACT (do
 *  not regress): STT is MORE sensitive than the TTS text leg — when the cloud-STT
 *  tier is on, the user's VOICE AUDIO (their actual recording) LEAVES the device to
 *  be transcribed by ElevenLabs Scribe. On-device whisper (mlx_whisper) is the
 *  private/offline DEFAULT and the FALLBACK on any cloud error. Like the other
 *  daemon-owned subsystems it is configured in darwin.toml under [voice].cloud_stt
 *  (the HUD never writes daemon config) and ships OFF, so this section REFLECTS the
 *  live stt.tier verdict + documents the exact pinned key rather than offering a
 *  toggle that would silently do nothing. Live transcription quality is
 *  device/credential-gated and is NEVER claimed measured here. */
function SttTierSection({ sttTier }: { sttTier: SttTierStatus }) {
  const tone = sttTierTone(sttTier.backend);
  const label = sttTierLabel(sttTier.backend);

  return (
    <>
      <div className="cred-row">
        <div className="cred-label">Transcription</div>
        <div className="cred-hint">
          Picks WHICH backend transcribes your captured audio. ON-DEVICE STT
          (whisper / mlx_whisper) is the private/offline default + the fallback on
          any cloud error. CLOUD STT (ElevenLabs Scribe) sends your VOICE AUDIO to
          the cloud — MORE sensitive than the TTS text leg, because it is your actual
          voice recording, not synthesized output. {sttTierDetail(sttTier.backend)}
        </div>
        <div className={`cred-pill ${tone}`}>{label}</div>
      </div>

      <div className="kv-note">
        Cloud STT is configured in <code>darwin.toml</code> under{" "}
        <code>[voice]</code> (the HUD does not write daemon config) and ships OFF.
        Key:
        <ul className="voiceid-keys">
          <li>
            <code>cloud_stt</code> — the SEPARATE master switch for cloud
            transcription (default <code>false</code>), distinct from{" "}
            <code>cloud_tier</code> (cloud TTS). When off — or with no ElevenLabs
            key, offline, or the model tier pinned LOCAL/&quot;work offline&quot; —
            transcription stays on-device whisper, exactly like today.
          </li>
        </ul>
        Cloud STT engages ONLY when it is enabled AND a key is present AND the model
        tier is not LOCAL. On ANY Scribe error or missing key the daemon falls back
        to on-device whisper, so a turn is never lost. The indicator above reflects
        the live <code>stt.tier</code> telemetry (which backend transcribed) and
        carries NO key or transcript. HONEST: turning cloud STT on means your VOICE
        AUDIO leaves the device — on-device whisper stays the private/offline default.
      </div>
    </>
  );
}

/** MODEL TIER // WHICH MODEL ANSWERS — the review + control surface for the
 *  model-tier layer (daemon/src/model_tier.rs + router.rs). HONESTY CONTRACT (do
 *  not regress): this is MODEL-ONLY — it picks WHICH model answers and changes NO
 *  safety gate (the consequential confirmation gate, the [integrations] master
 *  switch, the owner voice-id gate, and per-agent allowlists are identical at
 *  every tier). LOCAL means NO cloud call — the utterance + content stay on-device
 *  (a REAL privacy benefit) — but the on-device model is the resident ~4B with a
 *  genuine CAPABILITY CEILING: LOCAL is NOT Opus-grade, and this copy never implies
 *  it is. AUTO is a per-turn difficulty HEURISTIC: it can be wrong, is overridable,
 *  and is surfaced. FAST/HEAVY are cloud tiers (a cloud key + reachability are
 *  required); when the cloud is unreachable the resolver degrades to local
 *  (reason=FALLBACK), never a silent wrong answer.
 *
 *  The runtime override is what these buttons set — by sending the SAME spoken
 *  model-control command the voice path uses (an `ask` over the command channel,
 *  which the daemon's conservative classify_model_swap interprets), so the HUD adds
 *  no new authority and the override + telemetry flow are identical to voice. The
 *  DURABLE default lives in darwin.toml under [router].conversation_route (the HUD
 *  never writes daemon config); the runtime override resets to that default on
 *  restart. "Auto" clears the override back to that default. */
function ModelTierSection({
  modelTier,
  shell,
}: {
  modelTier: ModelTierStatus;
  shell: boolean;
}) {
  const tone = modelTierTone(modelTier.tier, modelTier.reason);
  const label = modelTierLabel(modelTier.tier);
  const mode = modelTierModeLabel(modelTier.manual);
  const reasonLabel = modelTierReasonLabel(modelTier.reason);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");

  // Each control sends the canonical spoken model-control phrase over the command
  // channel; the daemon's classify_model_swap installs/clears the override and
  // emits model.swap/model.tier (which refresh the live indicator). The HUD never
  // writes the override directly — it asks the daemon, exactly like voice.
  const swap = useCallback(
    async (intent: ModelSwapIntent, phrase: string) => {
      if (busy) return;
      setBusy(true);
      setNote("");
      try {
        const r = await sendCommand({ cmd: "ask", text: phrase });
        setNote(
          r.ok
            ? r.reply || `Requested: ${intent}.`
            : r.error || "command failed",
        );
      } catch {
        setNote("shell error");
      } finally {
        setBusy(false);
      }
    },
    [busy],
  );

  // The live status line under the section title — the same honest verdict the
  // StatusBar chip shows, expanded with the reason gloss when there is one.
  const statusDetail = reasonLabel ? ` — ${reasonLabel}` : "";

  return (
    <>
      <div className="cred-row">
        <div className="cred-label">Model tier</div>
        <div className="cred-hint">
          Picks WHICH model answers — MODEL-ONLY, it changes no safety gate. LOCAL
          = on-device, no cloud call (private), but capability-limited and NOT
          Opus-grade. AUTO picks per turn by a difficulty HEURISTIC (can be wrong,
          and you can override it). FAST/HEAVY are cloud tiers and need a cloud key;
          if the cloud is unreachable it degrades to on-device (FALLBACK).
          {modelTier.reason
            ? ` Now: ${modelTierHonest(modelTier.tier)} — ${modelTierReasonHonest(
                modelTier.reason,
              )}.`
            : ""}
        </div>
        <div className={`cred-pill ${tone}`}>
          {label} · {mode}
          {statusDetail}
        </div>
      </div>

      <div className="cred-row">
        <div className="cred-label">Set tier</div>
        <div className="modeltier-controls">
          <button
            className="icon-btn"
            onClick={() => void swap("heavy", MODEL_SWAP_BUTTON_PHRASES.heavy)}
            disabled={!shell || busy}
            title="Pin the cloud HEAVY model (most capable; needs a cloud key)"
          >
            HEAVY
          </button>
          <button
            className="icon-btn"
            onClick={() => void swap("fast", MODEL_SWAP_BUTTON_PHRASES.fast)}
            disabled={!shell || busy}
            title="Pin the cloud FAST model (quick + cheap; needs a cloud key)"
          >
            FAST
          </button>
          <button
            className="icon-btn"
            onClick={() => void swap("local", MODEL_SWAP_BUTTON_PHRASES.local)}
            disabled={!shell || busy}
            title="Pin the on-device model — NO cloud call (private), capability-limited"
          >
            LOCAL
          </button>
          <button
            className="icon-btn"
            onClick={() => void swap("auto", MODEL_SWAP_BUTTON_PHRASES.auto)}
            disabled={!shell || busy}
            title="Clear the override — AUTO picks per turn by a heuristic (the config default resumes)"
          >
            AUTO
          </button>
        </div>
        <div className="cred-hint">
          {note ||
            "Sends the same spoken model-control command the voice path uses. AUTO clears the override back to the config default."}
        </div>
      </div>

      <div className="kv-note">
        The DURABLE default lives in <code>darwin.toml</code> under{" "}
        <code>[router]</code> (the HUD does not write daemon config). Keys:
        <ul className="voiceid-keys">
          <li>
            <code>conversation_route</code> — the default tier:{" "}
            <code>&quot;cloud_heavy&quot;</code> (Opus, the shipped default),{" "}
            <code>&quot;cloud_fast&quot;</code> (Haiku), or{" "}
            <code>&quot;local&quot;</code> (the on-device model). An unknown value
            falls back to <code>local</code> (the safe, always-available default).
          </li>
          <li>
            <code>cloud_confidence_threshold</code> — below this classifier
            confidence a turn is treated as harder, so AUTO steps up to a more
            capable tier. This is a routing HEURISTIC, not a measured accuracy.
          </li>
        </ul>
        The buttons above set a RUNTIME override (it resets to the{" "}
        <code>conversation_route</code> default on restart); say{" "}
        <b>&quot;use the most powerful model&quot;</b>,{" "}
        <b>&quot;fast mode&quot;</b>, <b>&quot;work offline&quot;</b>, or{" "}
        <b>&quot;auto&quot;</b> for the same effect by voice. Going LOCAL/offline is
        a REAL privacy choice — the utterance and content stay on-device with no
        cloud call — but the on-device model is capability-limited and is NOT a
        substitute for the heavy cloud model on hard tasks. Every tier is subject to
        the SAME confirmation gate, master switch, and voice-id gate; the swap
        changes the model only.
      </div>
    </>
  );
}

/** MEMORY // EPISODES + USER MODEL — the retention + clear-memory documentation
 *  surface. HONESTY CONTRACT (do not regress): DARWIN's episodic store + user
 *  model are built ONLY from observed interactions, are REDACTED + LOCAL +
 *  AGENT-SCOPED, and are BOUNDED (evict-oldest at the cap) — NOT "remembers
 *  everything". Like voice-id + MCP, the store is configured in darwin.toml under
 *  [episodic] (the HUD never writes daemon config), so this section REFLECTS the
 *  exact config keys + the spoken/HUD clear controls rather than offering a
 *  toggle that would silently do nothing. The key names below are byte-for-byte
 *  the daemon's [episodic] keys (config.rs: enabled / retention). Nothing here is
 *  persisted client-side beyond what the daemon already stores. */
function MemorySection() {
  return (
    <>
      <div className="cred-row">
        <div className="cred-label">Memory</div>
        <div className="cred-hint">
          Episodes and the user model are built ONLY from real, observed turns,
          REDACTED before storage, kept LOCAL in the daemon, and SCOPED to the
          handling agent. Retention is BOUNDED — oldest evicted at the cap — so
          this is never &ldquo;remembers everything.&rdquo; The model is observed,
          not certain: it can be wrong, and you can correct or forget it.
        </div>
        <div className="cred-pill on">LOCAL · BOUNDED</div>
      </div>

      <div className="kv-note">
        Episodic memory is configured in <code>darwin.toml</code> under{" "}
        <code>[episodic]</code> (the HUD does not write daemon config). Keys:
        <ul className="voiceid-keys">
          <li>
            <code>enabled</code> — master switch for the episodic store. When off,
            no episodes are recorded and recall returns nothing.
          </li>
          <li>
            <code>retention</code> — the bounded evict-oldest cap on stored
            episodes. The newest fit within the cap; older ones are evicted on the
            retention pass. This is a real bound, never unbounded. Restart darwind
            after editing.
          </li>
        </ul>
        A turn is recorded ONLY when it is a real exchange — a transient
        screen-read, an empty/abandoned turn, or (with voice-id enabled) an
        unverified speaker is NOT recorded. The episode TIMELINE and the observed
        user-model profile are on the MEMORY panel.
      </div>

      <div className="kv-note">
        CLEARING MEMORY is spoken or done on the MEMORY panel — never automatic,
        and each tier is independent:
        <ul className="voiceid-keys">
          <li>
            <b>User model</b> — say{" "}
            <b>&ldquo;forget what you know about me&rdquo;</b> (or use FORGET on the
            MEMORY panel) to clear the whole observed profile; say{" "}
            <b>&ldquo;that&rsquo;s wrong, &hellip;&rdquo;</b> to correct one entry.
          </li>
          <li>
            <b>Episodes</b> — say{" "}
            <b>&ldquo;forget our recent conversations&rdquo;</b> to clear the
            episodic store for the current scope.
          </li>
        </ul>
        Each clear touches only its own tier (the user model, episodes, the world
        model, and stored facts have separate forget paths) and the daemon reports
        honestly how much it cleared. You are always in control.
      </div>
    </>
  );
}

/** ENCRYPTION // AT-REST ON DISK — the encryption status indicator + the enable
 *  control/documentation for the opt-in, ships-OFF whole-file SQLCipher encryption
 *  (daemon/src/crypto.rs + the per-store open_encrypted seams). HONESTY CONTRACT (do
 *  not regress, and do not OVERCLAIM):
 *    - The indicator reads ENCRYPTED AT REST / NOT ENCRYPTED from the GROUND-TRUTH
 *      `active` (the master key actually RESOLVED this run) — NEVER from `config`
 *      alone. A config-on-but-key-failed session reads honestly as NOT ENCRYPTED.
 *    - SCOPE is PARTIAL and stated EXACTLY: ENCRYPTED = the four sensitive SQLite
 *      stores (main Db, docsearch.db, audit.db, the optimize trace store) + the
 *      voiceid owner profile (its own encrypted SQLCipher blob). NOT ENCRYPTED =
 *      the config TOML, the Keychain item itself (already OS-protected), and —
 *      critically — the in-RAM working set + decrypted pages + the key WHILE the
 *      daemon runs. We never say "all your data is encrypted".
 *    - It protects AT REST ON DISK only — NOT against a live-process/root attacker.
 *    - The 256-bit master key lives ONLY in the macOS Keychain (account
 *      memory_encryption_key); lose it and the encrypted DBs are unrecoverable.
 *    - It SHIPS OFF and enabling CHANGES THE ON-DISK FORMAT (a one-time
 *      plaintext->encrypted migration on the next start).
 *
 *  Like every other daemon-owned subsystem (voice-id / MCP / memory / docsearch /
 *  cloud-STT), encryption is configured in darwin.toml under [security] — the HUD
 *  NEVER writes daemon config and NEVER holds the key, so this section REFLECTS the
 *  live security.status posture and documents the EXACT config key + the enable
 *  steps, rather than offering a toggle that would silently do nothing (or, worse,
 *  pretend the HUD could touch the key). The key name below is byte-for-byte the
 *  daemon's [security] key (config.rs KNOWN_KEYS: encrypt_memory). The `security`
 *  prop is the secret-free security.status snapshot — it never carries the key. */
/** PANIC // EMERGENCY STOP (LOCKDOWN) — task #12, item (4). The observable face of
 *  the daemon's lockdown emergency stop. It READS the live posture from the
 *  lockdown.status prop (LOCKED DOWN / NORMAL) and WRITES through the DEDICATED
 *  command-channel verbs `{cmd:"panic"}` / `{cmd:"unlock"}` — NEVER `{cmd:"ask"}`,
 *  so a panic can never leak to the model/answer path. The daemon calls
 *  lockdown::panic()/unlock() DIRECTLY for these verbs (not the model tool loop);
 *  unlock is the authenticated-local USER path and there is no agent/model route
 *  to it.
 *
 *  HONEST copy, echoed VERBATIM from the daemon consts (PANIC_CONFIRMATION /
 *  UNLOCK_CONFIRMATION): panic stops ALL future outward actions + autonomy + the
 *  mic immediately and PERSISTS across a restart; it does NOT undo anything
 *  already done (a sent message stays sent). Unlock is DELIBERATE (a two-step
 *  confirm here) and user-only, and RESTORES your configured settings — lockdown
 *  is an overlay, so nothing was changed underneath them.
 *
 *  The reply carries `locked`; we fold it into the HUD indicator immediately via
 *  onLockedChange so the chip flips on the press. This component carries NO
 *  authority of its own — it relays the user's explicit press; the daemon does the
 *  stop/lift. */
function LockdownSection({
  lockdown,
  shell,
  onLockedChange,
}: {
  lockdown: LockdownStatus | null;
  shell: boolean;
  onLockedChange?: (locked: boolean) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");
  // UNLOCK is deliberate: a first click ARMS the confirm, a second click commits.
  // Engaging panic is intentionally one click (the more it can fire, the safer).
  const [unlockArmed, setUnlockArmed] = useState(false);

  const locked = lockdown?.locked ?? false;
  const tone = lockdown === null ? "idle" : lockdownTone(lockdown);
  const label = lockdown === null ? "AWAITING" : lockdownLabel(lockdown);
  const restored = lockdown?.restoredFromMarker ?? false;

  // Engage the emergency stop: the DEDICATED panic verb (NOT ask). Echo the
  // daemon's own ack (the reply), and flip the indicator from the reply's locked.
  const panic = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    setNote("");
    setUnlockArmed(false);
    try {
      const r = await sendCommand({ cmd: "panic" });
      setNote(r.ok ? r.reply || PANIC_CONFIRMATION : r.error || "command failed");
      if (r.ok && typeof r.locked === "boolean") onLockedChange?.(r.locked);
    } catch {
      setNote("shell error");
    } finally {
      setBusy(false);
    }
  }, [busy, onLockedChange]);

  // Lift the stop: the DEDICATED unlock verb (the user-only resume). Two-step:
  // the first press arms, the second commits — so an unlock is always deliberate.
  const unlock = useCallback(async () => {
    if (busy) return;
    if (!unlockArmed) {
      setUnlockArmed(true);
      setNote(
        "Confirm: unlock lifts the emergency stop and restores your configured " +
          "settings. Click UNLOCK again to confirm.",
      );
      return;
    }
    setBusy(true);
    setNote("");
    try {
      const r = await sendCommand({ cmd: "unlock" });
      setNote(r.ok ? r.reply || UNLOCK_CONFIRMATION : r.error || "command failed");
      if (r.ok && typeof r.locked === "boolean") onLockedChange?.(r.locked);
    } catch {
      setNote("shell error");
    } finally {
      setBusy(false);
      setUnlockArmed(false);
    }
  }, [busy, unlockArmed, onLockedChange]);

  return (
    <>
      <div className="cred-row">
        <div className="cred-label">Emergency stop</div>
        <div className="cred-hint">
          PANIC is the kill switch. It immediately stops <b>all future</b> outward
          actions, <b>all</b> autonomy (proactive speech, standing tasks,
          self-heal, the app forge, MCP), and the <b>microphone</b> — and it{" "}
          <b>persists across a restart</b> until you deliberately unlock. HONEST:
          it does <b>NOT</b> undo anything already done — a message that was
          already sent stays sent; it can only stop what hasn&apos;t happened yet.
          Unlock is user-only and deliberate, and <b>restores your configured
          settings</b> exactly (lockdown is an overlay — nothing is changed
          underneath them). With lockdown off (the shipped default) every gate
          behaves exactly as today.
          {restored
            ? " This lockdown was RESTORED from the persisted marker on the last start — it survived a restart."
            : ""}
        </div>
        <div className={`cred-pill ${tone}`}>{label}</div>
      </div>

      <div className="cred-row">
        <div className="cred-label">Controls</div>
        <div className="modeltier-controls panic-controls">
          <button
            type="button"
            className="icon-btn panic-engage"
            onClick={() => void panic()}
            disabled={!shell || busy || locked}
            title="PANIC — engage the emergency stop now. Stops all future outward actions + autonomy + the mic immediately and persists across a restart. Does NOT undo anything already done."
          >
            ⛔ PANIC — STOP EVERYTHING
          </button>
          <button
            type="button"
            className={`icon-btn panic-unlock${unlockArmed ? " armed" : ""}`}
            onClick={() => void unlock()}
            disabled={!shell || busy || !locked}
            title="UNLOCK — deliberately lift the emergency stop (user-only). Restores your configured settings; nothing was changed underneath them. Two clicks to confirm."
          >
            {unlockArmed ? "✔ CONFIRM UNLOCK" : "🔓 UNLOCK"}
          </button>
        </div>
        <div className="cred-hint">
          {note ||
            (locked
              ? "Lockdown is ENGAGED. Click UNLOCK twice to lift it; unlock is deliberate and user-only — there is no agent or model path to it."
              : "Click PANIC to engage the emergency stop immediately. You can also say “panic” / “lockdown” / “stop everything” aloud, even mid-anything.")}
        </div>
      </div>

      <div className="kv-note">
        How the stop works (and its honest limits):
        <ul className="voiceid-keys">
          <li>
            ENGAGE is reachable from EVERYWHERE (the more it can fire, the safer):
            this button (the DEDICATED <code>panic</code> verb — never routed
            through the model), or saying <b>&ldquo;panic&rdquo;</b> /{" "}
            <b>&ldquo;lockdown&rdquo;</b> / <b>&ldquo;stop everything&rdquo;</b> /{" "}
            <b>&ldquo;kill switch&rdquo;</b>, honored BEFORE normal routing.
          </li>
          <li>
            While ENGAGED, every consequential / outward / autonomy / mic surface
            is forced OFF — no exception — and the stop is written to disk so a{" "}
            <b>restart re-enters lockdown</b> until you unlock.
          </li>
          <li>
            It is a force-OFF OVERLAY: your individual config switches are NOT
            touched, so <b>unlock restores them exactly</b>.
          </li>
          <li>
            HONEST: panic stops <b>future</b> actions; it cannot undo an action
            already executed. Unlock is <b>user-only + deliberate</b> — never
            automatic, never triggered by an agent, the model, or injected text.
          </li>
        </ul>
      </div>
    </>
  );
}

function EncryptionSection({ security }: { security: SecurityStatus | null }) {
  // Before the daemon emits the startup snapshot, render the honest AWAITING state
  // rather than a guessed posture. Once present, the indicator is driven by the
  // GROUND-TRUTH `active`, never `config` alone.
  const tone = security === null ? "idle" : securityTone(security);
  const label = security === null ? "AWAITING" : securityLabel(security);
  // The honest "config on but key did not resolve" affordance — the indicator is
  // NOT ENCRYPTED and the reason is a key/Keychain failure, not the OFF default.
  const mismatch = security !== null && security.config && !security.active;
  // Prefer the daemon's verbatim scope arrays + honesty copy (single source of
  // truth); fall back to the built-in lists for an older daemon.
  const encryptedStores =
    security !== null && security.encryptedStores.length > 0
      ? security.encryptedStores
      : [
          "the main Db (facts / transcripts / episodes / events + world-model facts)",
          "the docsearch index (chunk text + vectors)",
          "the audit log (the hash-chained ledger)",
          "the optimizer trace store",
          "the voiceid owner profile (a JSON feature vector, wrapped in its own encrypted blob)",
        ];
  const notEncrypted =
    security !== null && security.notEncrypted.length > 0
      ? security.notEncrypted
      : [
          "the config TOML (darwin.toml)",
          "the macOS Keychain item itself (already OS-protected)",
          "the in-RAM working set + decrypted pages + the key while darwind runs",
        ];

  return (
    <>
      <div className="cred-row">
        <div className="cred-label">At-rest encryption</div>
        <div className="cred-hint">
          Encrypts the sensitive on-disk stores with transparent whole-file
          SQLCipher AES-256 (page-level). HONEST: this protects your data{" "}
          <b>at rest on disk only</b> — it does NOT defend against a
          live-process or root attacker, because while darwind runs the key and
          the decrypted pages are in RAM. Scope is partial (below): the config
          TOML and the in-RAM working set are NOT encrypted. Ships OFF; enabling
          changes the on-disk format (a one-time migration).
          {mismatch
            ? " Right now the config requests encryption but the master key did NOT resolve, so the stores are still PLAINTEXT — check the Keychain item."
            : ""}
        </div>
        <div className={`cred-pill ${tone}`}>{label}</div>
      </div>

      <div className="kv-note">
        ENCRYPTED (transparent whole-file SQLCipher AES-256, page-level) — opened
        via a per-store <code>open_encrypted</code>:
        <ul className="voiceid-keys">
          {encryptedStores.map((line) => (
            <li key={line}>{line}</li>
          ))}
        </ul>
        EXPLICITLY <b>NOT</b> encrypted:
        <ul className="voiceid-keys">
          {notEncrypted.map((line) => (
            <li key={line}>{line}</li>
          ))}
        </ul>
      </div>

      <div className="kv-note">
        Encryption is configured in <code>darwin.toml</code> under{" "}
        <code>[security]</code> (the HUD never writes daemon config and never
        holds the key). Key:
        <ul className="voiceid-keys">
          <li>
            <code>encrypt_memory</code> — the master switch. Ships <b>false</b>;
            with it off EVERY store opens via its plaintext{" "}
            <code>open(path)</code> with no <code>PRAGMA key</code> —
            byte-for-byte today&apos;s plaintext SQLite (no key, no migration, no
            behavior change).
          </li>
        </ul>
        TO ENABLE: set <code>encrypt_memory = true</code> under{" "}
        <code>[security]</code> and restart darwind. On that next start the daemon
        generates a fresh 256-bit master key, stores it in the macOS Keychain
        (account <code>memory_encryption_key</code>), and RE-KEYS the existing
        plaintext stores to encrypted (a one-time read-plaintext -&gt;
        write-encrypted migration). The key is NEVER logged, shown here, on argv,
        or in telemetry. HONEST: the key lives ONLY in the Keychain — if you lose
        that item, the encrypted DBs are unrecoverable. The indicator above
        reflects the live <code>security.status</code> telemetry and reads{" "}
        ENCRYPTED AT REST only when the key ACTUALLY resolved this run (not just
        when the config flag is on).
      </div>
    </>
  );
}

/** FILE SEARCH // ON-DEVICE — reflects the [docsearch] config keys + the spoken
 *  index/search/forget controls and the live index status. Like the MEMORY/MCP
 *  sections, the HUD never writes daemon config and never triggers a disk read,
 *  so this REFLECTS the exact keys rather than offering a toggle that would
 *  silently do nothing. Honest copy: 100% on-device, allowlist-only (ships OFF,
 *  never a whole-disk scan), text-like files only in v1 (PDFs skipped), bounded,
 *  forgettable, and search falls back to BM25 when the on-device embedder is
 *  down. The key names below are byte-for-byte the daemon's [docsearch] keys
 *  (config.rs). `docIndex` (docsearch.indexed telemetry) shows the live size +
 *  whether search runs neural or BM25 — counts only, never a path. */
function DocSearchSection({ docIndex }: { docIndex: DocIndexStatus | null }) {
  const indexed = docIndex !== null && docIndex.chunks > 0;
  const fullyEmbedded = indexed && docIndex!.embeddedChunks === docIndex!.chunks;
  return (
    <>
      <div className="cred-row">
        <div className="cred-label">File search</div>
        <div className="cred-hint">
          Search your OWN files by meaning, 100% on-device. It reads ONLY the
          folders you allowlist (it ships OFF and never scans your whole disk),
          and your file contents + embeddings NEVER leave this machine — the
          embedder is the on-device model. Results CITE real indexed files; when
          the on-device embedder is down, search falls back to keyword (BM25)
          ranking and says which ran. Text-like files only in v1 — PDFs are
          skipped (they need a parser), never silently indexed. The index is
          BOUNDED and FORGETTABLE.
        </div>
        <div className="cred-pill on">ON-DEVICE · BOUNDED</div>
      </div>

      <div className="kv-note">
        File search is configured in <code>darwin.toml</code> under{" "}
        <code>[docsearch]</code> (the HUD does not write daemon config). Keys:
        <ul className="voiceid-keys">
          <li>
            <code>enabled</code> — master switch. Ships <b>false</b>; with it off
            the indexer is inert (no walk, no read, no embed, no store).
          </li>
          <li>
            <code>roots</code> — the EXPLICIT allowlist of folders that may be
            indexed. Ships <b>empty</b>; even with <code>enabled</code> true an
            empty <code>roots</code> indexes nothing. Every file is path-confined
            (a symlink-escape / <code>..</code> / outside-root file is rejected),
            so the index can never reach a file outside an allowlisted folder.
          </li>
          <li>
            <code>max_files</code> / <code>max_chunks</code> /{" "}
            <code>max_file_bytes</code> / <code>max_depth</code> /{" "}
            <code>chunk_chars</code> / <code>chunk_overlap</code> — the bounds
            that keep the on-disk index finite. Hidden + binary +
            non-text-like-extension files are skipped regardless. Restart darwind
            after editing.
          </li>
        </ul>
      </div>

      <div className="kv-note">
        Indexing, searching, and clearing are SPOKEN (the HUD never triggers a
        disk read):
        <ul className="voiceid-keys">
          <li>
            Say <b>&ldquo;index my documents&rdquo;</b> (or{" "}
            <b>&ldquo;reindex&rdquo;</b>) to (re)build the index over your
            allowlisted folders.
          </li>
          <li>
            Say <b>&ldquo;search my files for &hellip;&rdquo;</b> to query it —
            the answer cites the real files it came from on the FILE SEARCH panel.
          </li>
          <li>
            Say <b>&ldquo;forget my file index&rdquo;</b> to clear the whole index
            (it is rebuilt only when you index again).
          </li>
        </ul>
        {indexed ? (
          <div className="kv-status">
            Index status: <b>{docIndex!.files}</b> file(s),{" "}
            <b>{docIndex!.chunks}</b> chunk(s),{" "}
            <b>{docIndex!.embeddedChunks}</b> embedded on-device —{" "}
            {fullyEmbedded
              ? "search runs NEURAL (cosine over on-device embeddings)."
              : "some chunks are not embedded, so search over them falls back to BM25; reindex with the on-device model up to make it fully neural."}
          </div>
        ) : (
          <div className="kv-status">
            Index status: not built yet. Enable <code>[docsearch]</code>,
            allowlist a folder, and say &ldquo;index my documents&rdquo;.
          </div>
        )}
      </div>
    </>
  );
}

/** CONSEQUENTIAL POLICY // PER-ACTION ALLOW / NEVER / ASK — the user-facing
 *  editor for the crown-jewel gate's per-action policy (daemon/src/policy.rs). It
 *  READS the user-set rules from the policy.snapshot prop (deterministic order)
 *  and WRITES through the DEDICATED command-channel verb (`{cmd:"policy", text}`
 *  with the anchored POLICY_PHRASES) — NOT `ask`, so the phrase never reaches the
 *  model. The daemon's policy verb classifier (`policy::classify_policy_command`)
 *  turns it into PolicyStore::set / clear via the user-only `policy::apply_global`.
 *
 *  USER-SET ONLY: every write here is an EXPLICIT user action (a click, exactly
 *  like speaking the phrase after voice-id). There is NO agent/model/tool path
 *  that can reach set/clear — an injected "set policy allow X" cannot fire because
 *  the only writers are the authenticated-local `policy` verb, the post-voice-id
 *  spoken classifier, and the startup file load; the model tool loop holds only an
 *  immutable `&PolicyStore` and has no policy-write tool. This component carries no
 *  authority of its own; it asks the daemon, which still enforces the master
 *  switch + voice-id + confirmation backstops.
 *
 *  HONESTY (surfaced in copy): ALWAYS is a deliberate, MASTER-GATED loosening
 *  (inert when the master switch is OFF — it can NEVER override the master); NEVER
 *  always wins (even master ON + a fresh confirmation); ASK (the default, and the
 *  empty-store behavior) parks for the spoken confirmation. The shipped default is
 *  an EMPTY store — ASK everywhere — so behavior is exactly today's gate. */
function PolicySection({
  policy,
  shell,
}: {
  policy: PolicySnapshot | null;
  shell: boolean;
}) {
  const [tool, setTool] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");

  const rules = policy?.rules ?? [];
  const enabled = policy?.enabled ?? false;

  // One write = one explicit user action. Sends the anchored phrase over the
  // command channel; the daemon classifies it into set/clear. The HUD adds NO
  // authority — it relays the user's explicit choice exactly like the voice path.
  const send = useCallback(
    async (decision: PolicyDecision, forTool: string) => {
      const clean = forTool.trim();
      if (!clean || busy) return;
      setBusy(true);
      setNote("");
      try {
        const phrase = POLICY_PHRASES[decision](clean);
        // The DEDICATED `policy` verb (NOT `ask`): the daemon classifies the
        // anchored phrase and applies it via the USER-SET-ONLY write path; it
        // never reaches the model tool loop. The reply is the daemon's own ack
        // (what it actually did), so we surface that rather than a bare "Sent".
        const r = await sendCommand({ cmd: "policy", text: phrase });
        setNote(
          r.ok
            ? r.reply || "Policy updated."
            : r.error || "command failed",
        );
        if (r.ok) setTool("");
      } catch {
        setNote("shell error");
      } finally {
        setBusy(false);
      }
    },
    [busy],
  );

  return (
    <>
      <div className="cred-row">
        <div className="cred-label">Per-action policy</div>
        <div className="cred-hint">
          Set what DARWIN does when a CONSEQUENTIAL tool wants to act: <b>ALWAYS</b>{" "}
          (auto-approve — a deliberate loosening that STILL needs the master switch
          ON and your voice; it is inert when the master switch is off and can never
          override it), <b>NEVER</b> (a hard block that always wins, even with the
          master on and a fresh confirmation), or <b>ASK</b> (the default: park for
          a spoken confirmation). Rules are user-set only — there is no agent or
          model path that can change one.
        </div>
        <div className={`cred-pill ${enabled ? "on" : "idle"}`}>
          {policy === null
            ? "AWAITING"
            : enabled
              ? rules.length === 0
                ? "EMPTY · ASK EVERYWHERE"
                : `${rules.length} RULE${rules.length === 1 ? "" : "S"}`
              : "POLICY OFF · ASK EVERYWHERE"}
        </div>
      </div>

      {/* The current user-set rules (read-only listing; each row can be cleared
          back to ASK via the command channel). */}
      {rules.length > 0 && (
        <div className="policy-rules">
          {rules.map((rule) => (
            <PolicyRuleRow
              key={ruleKey(rule)}
              rule={rule}
              shell={shell}
              busy={busy}
              onAsk={() => void send("ask", rule.tool)}
            />
          ))}
        </div>
      )}

      {/* Add / change a rule for a tool the user names. The three buttons send
          the anchored phrases; the daemon does the set. */}
      <div className="cred-row">
        <div className="cred-label">Set rule</div>
        <input
          className="cred-input"
          type="text"
          autoComplete="off"
          autoCorrect="off"
          autoCapitalize="off"
          spellCheck={false}
          placeholder="consequential tool name, e.g. gmail_send"
          value={tool}
          onChange={(e) => setTool(e.target.value)}
          disabled={!shell || busy}
        />
        <div className="modeltier-controls policy-controls">
          <button
            className="icon-btn policy-always"
            onClick={() => void send("always", tool)}
            disabled={!shell || busy || tool.trim().length === 0}
            title="ALWAYS — auto-approve, but ONLY when the master switch is ON and voice-id allows. Inert with the master off; never overrides the master."
          >
            ALWAYS ALLOW
          </button>
          <button
            className="icon-btn policy-never"
            onClick={() => void send("never", tool)}
            disabled={!shell || busy || tool.trim().length === 0}
            title="NEVER — a hard block that always wins, even with the master on and a fresh confirmation."
          >
            NEVER
          </button>
          <button
            className="icon-btn policy-ask"
            onClick={() => void send("ask", tool)}
            disabled={!shell || busy || tool.trim().length === 0}
            title="ASK — clear any rule back to the default park/confirm flow."
          >
            ASK (DEFAULT)
          </button>
        </div>
        <div className="cred-hint">
          {note ||
            "Names a consequential tool, then pick the rule. The HUD sends the same spoken phrase the voice path uses — the daemon sets the rule; the HUD never sets one itself."}
        </div>
      </div>

      <div className="kv-note">
        The policy is the controlled face of the consequential gate, and it is
        bounded by HARD invariants:
        <ul className="voiceid-keys">
          <li>
            The <code>[integrations].allow_consequential</code> master switch is the
            CEILING — a policy can never grant an action the master forbids. With the
            master OFF, an <b>ALWAYS</b> rule is inert and the action is still only a
            dry-run preview.
          </li>
          <li>
            <b>NEVER</b> always wins — it hard-blocks even with the master on and a
            fresh confirmation.
          </li>
          <li>
            Policies are <b>USER-SET ONLY</b>. They are written by you here (or by an
            explicit spoken command) and loaded from{" "}
            <code>state/policy.json</code> at startup — never by an agent, a model, or
            a tool. An injected &ldquo;set policy allow X&rdquo; cannot take effect.
          </li>
          <li>
            The empty store (the shipped default) means <b>ASK everywhere</b> — every
            consequential action parks for a spoken confirmation, exactly as it does
            today. The voice-id + confirmation gates remain the backstop on the ASK
            path and behind ALWAYS.
          </li>
        </ul>
        You can also set these by voice (e.g. &ldquo;always allow the gmail_send
        action&rdquo;) — the same explicit, user-only path.
      </div>
    </>
  );
}

/** One user-set policy rule row: the scope (tool [+agent] [+recipient]) and its
 *  decision, plus a Clear (→ ASK) affordance routed through the command channel.
 *  Read-only display otherwise — the HUD never mutates the rule directly. */
function PolicyRuleRow({
  rule,
  shell,
  busy,
  onAsk,
}: {
  rule: PolicyRule;
  shell: boolean;
  busy: boolean;
  onAsk: () => void;
}) {
  const decisionLabel =
    rule.decision === "always"
      ? "ALWAYS"
      : rule.decision === "never"
        ? "NEVER"
        : "ASK";
  return (
    <div className="policy-rule-row">
      <span className={`policy-decision ${rule.decision}`}>{decisionLabel}</span>
      <span className="policy-tool">{rule.tool}</span>
      {rule.agent ? (
        <span className="policy-scope-bit" title="scoped to this agent">
          agent: {rule.agent}
        </span>
      ) : null}
      {rule.recipient ? (
        <span className="policy-scope-bit" title="scoped to targets containing this">
          to: {rule.recipient}
        </span>
      ) : null}
      <button
        className="icon-btn cred-remove"
        onClick={onAsk}
        disabled={!shell || busy || rule.decision === "ask"}
        title="Clear this rule back to ASK (the default park/confirm)"
        aria-label={`clear policy rule for ${rule.tool}`}
      >
        ✕
      </button>
    </div>
  );
}

/** A stable React key for a rule row from its full scope (tool + optional
 *  agent/recipient), so two rules on the same tool with different scopes do not
 *  collide. */
function ruleKey(rule: PolicyRule): string {
  return `${rule.tool} ${rule.agent ?? ""} ${rule.recipient ?? ""}`;
}
