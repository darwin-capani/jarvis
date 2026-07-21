import type { CSSProperties } from "react";
import type { ActiveAgent } from "../core/state";
import { ROSTER } from "../core/agents";
import Frame from "./Frame";

/**
 * CONSTELLATION // AGENTS — the team layer panel (CONTRACT part C.2). Lists the
 * full 27-agent roster (name + role) seeded from the static mirror so the team
 * is visible immediately, with the ACTIVE agent highlighted in ITS OWN hue and
 * the rest dimmed. The active agent's hue is applied inline (it is a runtime
 * value 0..360 from agent.active, not a fixed token), so each agent lights in
 * its identity color — including ultron's deep-orange 15, which is NOT the
 * reserved alert-red.
 *
 * Anti-flicker: every per-row color/opacity change carries a CSS transition
 * (.agent-row in styles.css), so an agent lighting up or dimming is a fade,
 * never a single-frame cut. The full roster is always mounted (stable keys by
 * name); only the highlighted class + the inline hue change between turns.
 */
export default function AgentPanel({ active }: { active: ActiveAgent | null }) {
  const activeName = active?.name ?? null;
  const tag = active ? active.name.toUpperCase() : "STANDBY";

  return (
    <Frame className="constellation" title="CONSTELLATION // AGENTS" tag={tag}>
      <div className="agent-list">
        {ROSTER.map((a) => {
          const isActive = a.name === activeName;
          // The active agent renders in the LIVE hue from the event (which may
          // differ from the static mirror if the daemon roster shifts); idle
          // rows use their own static identity hue at low intensity.
          const hue = isActive ? active!.hue : a.hue;
          const style = {
            // CSS custom property consumed by .agent-row in styles.css.
            ["--agent-hue" as string]: String(hue),
          } as CSSProperties;
          return (
            <div
              key={a.name}
              className={`agent-row ${isActive ? "active" : "dim"}`}
              style={style}
            >
              <span className="agent-glyph" aria-hidden="true" />
              <span className="agent-name">{a.name.toUpperCase()}</span>
              <span className="agent-role">{a.role}</span>
            </div>
          );
        })}
      </div>
      {/* a11y: role="status" — which agent is HANDLING the turn is announced
          politely when it changes (an SR user otherwise never learns who
          answered). */}
      <div className={`agent-handling ${active ? "on" : ""}`} role="status">
        {active ? (
          <>
            <span className="ah-label">HANDLING</span>
            <span
              className="ah-name"
              style={{ ["--agent-hue" as string]: String(active.hue) } as CSSProperties}
            >
              {active.name.toUpperCase()}
            </span>
            {active.role ? <span className="ah-role">{active.role}</span> : null}
          </>
        ) : (
          <span className="ah-idle">PRIME ORCHESTRATOR ON STANDBY</span>
        )}
      </div>
    </Frame>
  );
}
