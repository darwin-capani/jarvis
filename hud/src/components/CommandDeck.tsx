import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import { ROSTER, normalizeHue, PRIME_AGENT } from "../core/agents";
import {
  AUTO_ROUTE,
  agentForAsk,
  deckReduce,
  forgeApplyCommand,
  hasPending,
  initialDeckState,
  parsePendingSnapshot,
  replyToActions,
} from "../core/deck";
import {
  sendCommand,
  type CommandReply,
  type PendingSnapshot,
} from "../tauri/command";
import Frame from "./Frame";
import useModalFocus from "./useModalFocus";

/**
 * COMMAND DECK — the interactive Iron-Man holotable command surface. The first
 * INBOUND surface in the HUD: a command input to talk to agents (pick one or let
 * Darwin-Prime route), one-tap brief / mission launchers, an interactive
 * constellation (the full roster as an addressable deck), and a PENDING-ACTIONS
 * TRAY that surfaces cross-turn confirmations (Approve/Deny) and forge proposals
 * (Review/Dismiss — Dismiss only, with the manual apply command shown).
 *
 * SAFETY POSTURE (do not regress — it mirrors the channel contract):
 *   - The deck adds NO authority. It only assembles bounded requests and renders
 *     bounded prose replies. Every consequential `ask` STILL parks daemon-side;
 *     the deck surfaces the park prompt and the parked action then appears in the
 *     tray for an explicit Approve — the deck never fires.
 *   - Approve = `confirm {id}` (the daemon re-checks the master switch + the
 *     agent allowlist); Deny = `deny {id}` (clears, fires nothing).
 *   - The forge row is REVIEW-ONLY: it shows `scripts/apply_forge.sh <ts>` and a
 *     Dismiss button. There is deliberately NO apply/deploy/install button.
 *   - The capability token never touches this layer — the Tauri backend holds it
 *     and injects it on the wire. Nothing here renders a secret/token.
 *   - Additive to the telemetry HUD: it is a toggleable overlay, so the orb and
 *     the existing panels are untouched when it is closed.
 *
 * HONESTY: this component + its wiring + state are real and hermetically tested.
 * The live R3F holotable animation of the constellation is DEVICE-GATED (the
 * headless preview suspends R3F); the constellation here is the addressable DOM
 * deck, not a claimed-measured live 3D render.
 */
export default function CommandDeck({
  open,
  onClose,
  initialPending,
}: {
  open: boolean;
  onClose: () => void;
  /** Test-only seed for the pending tray, so the tray's static render (and the
   *  forge manual-command / no-apply-button assertions) are verifiable headlessly
   *  under vitest. In the app this is undefined and the tray is populated solely
   *  by the read-only `pending` poll. */
  initialPending?: PendingSnapshot;
}) {
  const [state, dispatch] = useReducer(deckReduce, undefined, () => {
    const base = initialDeckState();
    return initialPending ? { ...base, pending: initialPending } : base;
  });
  const [input, setInput] = useState("");
  const [target, setTarget] = useState<string>(AUTO_ROUTE);
  const logRef = useRef<HTMLDivElement>(null);

  // a11y: trap + autofocus (lands on the agent select / input) + focus-restore;
  // Escape closes the deck. The deck stays MOUNTED while closed, so the trap
  // keys off `open`.
  const deckRef = useRef<HTMLDivElement>(null);
  useModalFocus(deckRef, onClose, open);

  // Auto-scroll the log to the newest line.
  useEffect(() => {
    const el = logRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [state.log]);

  /** Run one command round-trip and fold the reply into the log/tray. Guards
   *  against concurrent sends with the `busy` flag. `expectPending` routes a
   *  snapshot reply into the tray; `replyAgent` attributes a prose reply. */
  const run = useCallback(
    async (
      request: Parameters<typeof sendCommand>[0],
      opts: { expectPending?: boolean; replyAgent?: string | null } = {},
    ): Promise<CommandReply> => {
      dispatch({ type: "busy", busy: true });
      let reply: CommandReply;
      try {
        reply = await sendCommand(request);
      } catch {
        // sendCommand already swallows throws, but belt-and-suspenders: never
        // let a rejection blank the deck.
        reply = { ok: false, error: "command failed" };
      }
      for (const action of replyToActions(reply, {
        expectPending: opts.expectPending ?? false,
        replyAgent: opts.replyAgent ?? null,
      })) {
        dispatch(action);
      }
      dispatch({ type: "busy", busy: false });
      return reply;
    },
    [],
  );

  /** Refresh the pending tray (the `pending` command — replay-free, ids only). */
  const refreshPending = useCallback(async () => {
    const reply = await sendCommand({ cmd: "pending" });
    if (reply.ok && reply.pending) {
      dispatch({ type: "pending", snapshot: parsePendingSnapshot(reply.pending) });
    }
  }, []);

  // Poll the tray while the deck is open (a parked action from an `ask`, or a
  // forge proposal, surfaces here without re-asking). Read-only + bounded.
  useEffect(() => {
    if (!open) return;
    void refreshPending();
    const id = setInterval(() => void refreshPending(), 4000);
    return () => clearInterval(id);
  }, [open, refreshPending]);

  const submitAsk = useCallback(async () => {
    const text = input.trim();
    if (text.length === 0 || state.busy) return;
    const agent = agentForAsk(target);
    dispatch({ type: "command", agent: agent ?? null, text });
    setInput("");
    await run({ cmd: "ask", text, agent }, { replyAgent: agent ?? PRIME_AGENT });
    // A consequential ask parks daemon-side — surface it in the tray promptly.
    void refreshPending();
  }, [input, state.busy, target, run, refreshPending]);

  const onBrief = useCallback(async () => {
    if (state.busy) return;
    dispatch({ type: "system", text: "Requesting Edith's brief…" });
    await run({ cmd: "brief" }, { replyAgent: "edith" });
  }, [state.busy, run]);

  const onMission = useCallback(async () => {
    const goal = input.trim();
    if (goal.length === 0 || state.busy) return;
    dispatch({ type: "command", agent: "fury", text: `MISSION: ${goal}` });
    setInput("");
    await run({ cmd: "mission", goal }, { replyAgent: "fury" });
    void refreshPending();
  }, [input, state.busy, run, refreshPending]);

  const onApprove = useCallback(
    async (id: string) => {
      if (state.busy) return;
      const reply = await run({ cmd: "confirm", id });
      if (reply.ok) dispatch({ type: "clearConfirmation" });
      void refreshPending();
    },
    [state.busy, run, refreshPending],
  );

  const onDeny = useCallback(
    async (id: string) => {
      if (state.busy) return;
      const reply = await run({ cmd: "deny", id });
      if (reply.ok) dispatch({ type: "clearConfirmation" });
      void refreshPending();
    },
    [state.busy, run, refreshPending],
  );

  const onDismissForge = useCallback(
    async (ts: string) => {
      if (state.busy) return;
      const tsNum = Number(ts);
      if (!Number.isFinite(tsNum)) return;
      const reply = await run({ cmd: "dismiss_forge", ts: tsNum });
      if (reply.ok) dispatch({ type: "clearForge" });
      void refreshPending();
    },
    [state.busy, run, refreshPending],
  );

  if (!open) return null;

  const conf = state.pending.confirmation ?? null;
  const forgeTs = state.pending.forge_pending_ts ?? null;
  const trayActive = hasPending(state.pending);

  return (
    <div className="command-deck" role="dialog" aria-label="Command Deck" aria-modal="true" ref={deckRef}>
      <Frame className="cmd-deck-frame" title="COMMAND DECK" tag="HOLOTABLE">
        <div className="cmd-deck-body">
          <div className="cmd-deck-head">
            <span className="cmd-deck-sub">
              Address an agent or let Darwin-Prime route. Consequential actions
              park for your approval.
            </span>
            <button className="cmd-deck-close" onClick={onClose} aria-label="Close deck">
              ✕
            </button>
          </div>

          {/* Pending-actions tray: confirmations (Approve/Deny) + forge
              proposals (Review/Dismiss only). Hidden when nothing is pending. */}
          {trayActive && (
            <div className="cmd-tray" aria-label="Pending actions">
              <div className="cmd-tray-label">PENDING ACTIONS</div>
              {conf && (
                <div className="cmd-tray-row cmd-tray-confirm" role="group">
                  <div className="cmd-tray-info">
                    <span className="cmd-tray-tool">{conf.tool}</span>
                    <span className="cmd-tray-agent">{conf.agent}</span>
                    <span className="cmd-tray-preview">{conf.preview}</span>
                  </div>
                  <div className="cmd-tray-actions">
                    <button
                      className="cmd-btn cmd-approve"
                      onClick={() => void onApprove(conf.id)}
                      disabled={state.busy}
                    >
                      APPROVE
                    </button>
                    <button
                      className="cmd-btn cmd-deny"
                      onClick={() => void onDeny(conf.id)}
                      disabled={state.busy}
                    >
                      DENY
                    </button>
                  </div>
                </div>
              )}
              {forgeTs && (
                <div className="cmd-tray-row cmd-tray-forge" role="group">
                  <div className="cmd-tray-info">
                    <span className="cmd-tray-tool">SELF-FORGE PROPOSAL</span>
                    <span className="cmd-tray-preview">
                      Review-only. To install, run the manual command (review
                      first). There is no auto-deploy.
                    </span>
                    <div className="cmd-forge-cmd" role="note">
                      <span className="cmd-forge-prompt" aria-hidden="true">
                        $
                      </span>
                      <code>{forgeApplyCommand(forgeTs)}</code>
                    </div>
                  </div>
                  <div className="cmd-tray-actions">
                    {/* DISMISS clears the proposal marker only — NEVER applies. */}
                    <button
                      className="cmd-btn cmd-dismiss"
                      onClick={() => void onDismissForge(forgeTs)}
                      disabled={state.busy}
                    >
                      DISMISS
                    </button>
                  </div>
                </div>
              )}
            </div>
          )}

          {/* The deck conversation log. */}
          <div className="cmd-log" ref={logRef} aria-live="polite">
            {state.log.length === 0 && (
              <div className="cmd-log-empty dim-note">
                No commands yet. Ask an agent, request a brief, or launch a
                mission.
              </div>
            )}
            {state.log.map((entry) => (
              <div key={entry.id} className={`cmd-log-row ${entry.kind}`}>
                {entry.agent && <span className="cmd-log-agent">{entry.agent}</span>}
                <span className="cmd-log-text">{entry.text}</span>
              </div>
            ))}
          </div>

          {/* Command input + agent selector + action launchers. */}
          <div className="cmd-input-row">
            <select
              className="cmd-agent-select"
              value={target}
              onChange={(e) => setTarget(e.target.value)}
              aria-label="Target agent"
            >
              <option value={AUTO_ROUTE}>Auto-route (Darwin-Prime)</option>
              {ROSTER.map((a) => (
                <option key={a.name} value={a.name}>
                  {a.name} — {a.role}
                </option>
              ))}
            </select>
            <input
              className="cmd-text"
              type="text"
              value={input}
              placeholder="Speak to the council…"
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") void submitAsk();
              }}
              aria-label="Command input"
              disabled={state.busy}
            />
            <button
              className="cmd-btn cmd-send"
              onClick={() => void submitAsk()}
              disabled={state.busy || input.trim().length === 0}
            >
              SEND
            </button>
          </div>

          <div className="cmd-launchers">
            <button className="cmd-btn cmd-brief" onClick={() => void onBrief()} disabled={state.busy}>
              BRIEF
            </button>
            <button
              className="cmd-btn cmd-mission"
              onClick={() => void onMission()}
              disabled={state.busy || input.trim().length === 0}
              title="Launch a bounded Fury mission from the input text"
            >
              LAUNCH MISSION
            </button>
            {state.busy && <span className="cmd-busy dim-note">working…</span>}
          </div>

          {/* The interactive constellation — the full roster as an addressable
              deck. Clicking a chip selects it as the ask target. (The live R3F
              holotable animation is device-gated; this is the DOM deck.) */}
          <div className="cmd-constellation" aria-label="Agent constellation">
            <div className="cmd-const-label">CONSTELLATION — {ROSTER.length} AGENTS</div>
            <div className="cmd-const-grid">
              {ROSTER.map((a) => {
                const selected = target === a.name;
                return (
                  <button
                    key={a.name}
                    className={`cmd-agent-chip ${selected ? "selected" : ""}`}
                    style={{ ["--agent-hue" as string]: String(normalizeHue(a.hue)) }}
                    onClick={() => setTarget(selected ? AUTO_ROUTE : a.name)}
                    aria-pressed={selected}
                    title={a.role}
                  >
                    <span className="cmd-chip-dot" aria-hidden="true" />
                    <span className="cmd-chip-name">{a.name}</span>
                  </button>
                );
              })}
            </div>
          </div>
        </div>
      </Frame>
    </div>
  );
}
