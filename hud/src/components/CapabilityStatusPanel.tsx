import type { Capability, CapabilityMap, CapStatus } from "../core/events";
import Frame from "./Frame";

/**
 * CAPABILITIES // ARMED · GATED — the live honest capability map (daemon
 * capability.rs -> `capability.map`). It makes DARWIN's "armed by default, gated
 * per action" posture visible: one row per notable subsystem, each honestly
 * labelled ready / needs-a-dependency / off.
 *
 * HONESTY CONTRACT (do not regress):
 *   - Never over-claims. A malformed frame coerces an unknown status to "off";
 *     the panel shows nothing until the first real frame (capabilityMap === null).
 *   - `verified` is surfaced: a green/amber pill is a status the daemon PROBED;
 *     an "unverified" marker means the requirement is real but was not live-checked
 *     here (an unread Keychain secret, a macOS TCC consent granted at first use).
 *     The UI must not present an unverified requirement as a confirmed one.
 *   - SECRET-FREE: `dependency` is a human phrase the daemon sent, never a value.
 */
export default function CapabilityStatusPanel({ map }: { map: CapabilityMap | null }) {
  // Mirror the other event-fed panels: render nothing until the first frame
  // arrives, rather than a fabricated "everything ready" placeholder.
  if (map === null || map.capabilities.length === 0) return null;

  return (
    <div className="capmap-panel">
      <Frame title="CAPABILITIES // ARMED · GATED" tag="LIVE · HONEST">
        <div className="capmap-body">
          {map.capabilities.map((c) => (
            <CapabilityRow key={c.key} cap={c} />
          ))}
          <div className="capmap-foot dim-note">
            Armed by default, gated per action: a subsystem ships ON but stays
            inert until its dependency is supplied, and every consequential action
            still requires a fresh spoken confirm. A dimmed{" "}
            <span className="capmap-unverified">unverified</span> tag means the
            requirement is real but not live-checked here (a Keychain secret this
            process doesn&rsquo;t read, or a macOS consent granted only at first use).
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One subsystem row: label, a status pill, and — when it needs a dependency —
 *  the human phrase plus an "unverified" marker when the daemon did not probe it. */
function CapabilityRow({ cap }: { cap: Capability }) {
  return (
    <div className="capmap-row">
      <span className="capmap-label">{cap.label}</span>
      <span className={`capmap-pill ${pillClass(cap.status)}`} title={pillTitle(cap)}>
        {pillText(cap.status)}
      </span>
      {cap.status === "armed_needs_dependency" && cap.dependency.length > 0 && (
        <span className="capmap-dep dim-note">
          needs: {cap.dependency}
          {!cap.verified && <span className="capmap-unverified"> · unverified</span>}
        </span>
      )}
    </div>
  );
}

function pillClass(s: CapStatus): string {
  switch (s) {
    case "ready":
      return "ready";
    case "armed_needs_dependency":
      return "needs-dep";
    default:
      return "off";
  }
}

function pillText(s: CapStatus): string {
  switch (s) {
    case "ready":
      return "READY";
    case "armed_needs_dependency":
      return "ARMED · NEEDS DEP";
    default:
      return "OFF";
  }
}

function pillTitle(c: Capability): string {
  switch (c.status) {
    case "ready":
      return "Armed and its dependency is satisfied (or it has none) — usable now.";
    case "armed_needs_dependency":
      return c.verified
        ? `Armed but inert: ${c.dependency} (the daemon confirmed this dependency is missing).`
        : `Armed but inert: ${c.dependency} (requirement stated; not live-checked here).`;
    default:
      return "This subsystem's own switch is off.";
  }
}
