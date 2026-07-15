import type { Presence } from "../core/events";
import Frame from "./Frame";

/**
 * PRESENCE // ATTENTION — the fused attention state (daemon presence.rs). Away /
 * Present / Focused, computed from input recency + (optional) speech + vision.
 * When Focused (silent flow) or Away, EDITH holds spoken proactivity — the panel
 * shows that honestly. Permission-neutral: this only ever quiets DARWIN.
 *
 * HONESTY: `signals` shows which raw inputs actually fed the fusion this tick.
 * Input recency feeds it today; speech + vision are wired but not yet sourced, so
 * the panel dims them rather than implying a reading that isn't there.
 */
export default function PresencePanel({ presence }: { presence: Presence | null }) {
  if (presence === null) return null;

  const suppressing = presence.state === "focused" || presence.state === "away";
  return (
    <div className="presence-panel">
      <Frame title="PRESENCE // ATTENTION" tag="EDITH">
        <div className="presence-body">
          <div className="presence-head">
            <span className={`presence-pill ${presence.state}`}>{label(presence.state)}</span>
            {suppressing && (
              <span className="presence-note dim-note">
                holding spoken proactivity (a silent card still surfaces)
              </span>
            )}
          </div>
          <div className="presence-signals dim-note">
            fusing:{" "}
            <SignalTag on={presence.signals.input} name="input" />{" "}
            <SignalTag on={presence.signals.speech} name="speech" />{" "}
            <SignalTag on={presence.signals.vision} name="vision" />
          </div>
        </div>
      </Frame>
    </div>
  );
}

function SignalTag({ on, name }: { on: boolean; name: string }) {
  return <span className={`presence-signal ${on ? "on" : "off"}`}>{name}</span>;
}

function label(s: Presence["state"]): string {
  switch (s) {
    case "focused":
      return "FOCUSED · IN FLOW";
    case "away":
      return "AWAY";
    default:
      return "PRESENT";
  }
}
