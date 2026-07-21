import AgentPanel from "./AgentPanel";
import CommandDeck from "./CommandDeck";
import DiagnosticsPanel from "./DiagnosticsPanel";
import GlobalScanPanel, { GLOBAL_SCAN_APP } from "./GlobalScanPanel";
import McpPanel from "./McpPanel";
import NexusPanel, { NEXUS_APP_NAME } from "./NexusPanel";
import SkillsPanel from "./SkillsPanel";
import VisionPanel, { VISION_APP } from "./VisionPanel";
import type { HudState } from "../core/state";

/**
 * TAKEOVER STAGE — the full-desktop, no-OS-chrome holographic mode the HUD
 * promotes into when the operator enters kiosk takeover. It COMPOSES (does not
 * fork) the existing surfaces: the live telemetry panels, the agent
 * constellation, and the SAME `CommandDeck` holotable the windowed HUD uses
 * (mounted open here, since the takeover desktop IS the command surface). The
 * windowed layout in App.tsx stays the default; this stage only mounts when
 * `takeoverActive` is true.
 *
 * THE HEADLINE SAFETY PROPERTY — exit is ALWAYS reachable. This stage renders a
 * VISIBLE, always-present "EXIT TAKEOVER" control UNCONDITIONALLY: there is no
 * prop, no state, and no branch in here that can hide it. Because App only mounts
 * this stage when takeover is active, the control's presence is exactly
 * coextensive with the active takeover (the `exitAlwaysReachable` invariant). It
 * is one of FOUR independent exits — alongside the Esc-key handler (App), the
 * backend reset-on-exit/Drop safety net, and (when available) an OS-level global
 * shortcut — so the operator can never be locked out of macOS.
 *
 * HONESTY: the actual fullscreen render + Dock/menu-bar hide is DEVICE-GATED (a
 * real Tauri app on a real display). What is proven hermetically is this layout +
 * the always-present EXIT control + the enter/exit state machine + the bridge
 * wiring (hud vitest) and the backend command/exit-safety (src-tauri cargo).
 */
export default function TakeoverStage({
  state,
  onExit,
}: {
  /** The live HUD state, so the takeover desktop reuses the same panels/feeds. */
  state: HudState;
  /** Wired to exitTakeover() — the always-available in-HUD escape hatch. */
  onExit: () => void;
}) {
  return (
    <div className="takeover-stage" role="region" aria-label="Kiosk takeover desktop">
      {/* ALWAYS-PRESENT EXIT AFFORDANCE. Rendered unconditionally — never behind
          a state that could hide it. This is the visible escape hatch (2d); Esc
          (App) is the keyboard escape hatch; the backend reset-on-exit/Drop and
          the optional global shortcut are the OS-level nets. */}
      <button
        type="button"
        className="takeover-exit"
        onClick={onExit}
        aria-label="Exit takeover"
        title="Exit takeover (or press Esc) — restores the Dock, menu bar, and window chrome"
      >
        ⤢ EXIT TAKEOVER
      </button>
      <span className="takeover-exit-hint" aria-hidden="true">
        Esc to exit
      </span>

      {/* The takeover desktop composes the live panels (reused, not forked). */}
      <div className="takeover-desktop">
        <div className="takeover-col takeover-col-left">
          <AgentPanel active={state.activeAgent} />
          <GlobalScanPanel
            feed={state.appFeeds[GLOBAL_SCAN_APP]}
            running={state.runningApps.has(GLOBAL_SCAN_APP)}
          />
          <VisionPanel
            feed={state.appFeeds[VISION_APP]}
            running={state.runningApps.has(VISION_APP)}
          />
        </div>

        {/* Center: the SAME CommandDeck holotable, mounted open. Reused verbatim
            — the takeover desktop IS the command surface. */}
        <div className="takeover-col takeover-col-center">
          <CommandDeck open onClose={onExit} inline />
        </div>

        <div className="takeover-col takeover-col-right">
          <NexusPanel
            feed={state.appFeeds[NEXUS_APP_NAME]}
            running={state.runningApps.has(NEXUS_APP_NAME)}
          />
          <McpPanel mcp={state.mcp} />
          <SkillsPanel skills={state.skills} />
          <DiagnosticsPanel gauges={state.gauges} facts={state.facts} actions={state.actions} />
        </div>
      </div>
    </div>
  );
}
