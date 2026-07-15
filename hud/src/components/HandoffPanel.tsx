import type { HandoffStatus } from "../core/events";
import Frame from "./Frame";

/**
 * HANDOFF // CONTINUITY — the honest state of resuming a LIVE session on another
 * of the owner's Macs (daemon handoff.rs).
 *
 * HONESTY CONTRACT (do not regress):
 *   - SHIPS OFF: OFF unless the operator enables [handoff] (it rides [sync],
 *     also OFF). The pill says so.
 *   - E2E-ENCRYPTED, SAME SEALED PATH: a session capsule never leaves the box
 *     unsealed; the transport is armed-but-inert. ARMED · NEEDS PAIRING until a
 *     shared key exists, then ARMED · PAIRED.
 *   - AUTHORITY NEVER TRANSFERS: the capsule carries NO resolved credential /
 *     bearer (redacted like a macro), and restoring context PARKS — every
 *     consequential step on the receiving Mac re-hits the fresh confirm +
 *     voice-id + master switch. The footnote says so plainly.
 *   - "Resume on <device>" is surfaced only when armed + paired; it is a
 *     REVIEW affordance (the resume is operator-triggered daemon-side and always
 *     parks), never an auto-action.
 */
export default function HandoffPanel({ handoff }: { handoff: HandoffStatus | null }) {
  if (handoff === null) return null;

  const state = pipelineState(handoff);
  const device = handoff.device.trim() || "your other Mac";
  return (
    <div className="handoff-panel">
      <Frame title="HANDOFF // CONTINUITY" tag="E2E · AUTHORITY STAYS">
        <div className="handoff-body">
          <div className="handoff-head">
            <span className={`handoff-pill ${state.cls}`}>{state.label}</span>
            {state.cls === "ready" && (
              <span className="handoff-resume dim-note">Resume on {device}</span>
            )}
          </div>
          {handoff.pendingCapsule && (
            <div className="handoff-pending">
              A session capsule from {device} is staged — restoring it repopulates
              context and parks; authority did not transfer.
            </div>
          )}
          <div className="handoff-foot dim-note">
            Resume a live session on your own Mac, end-to-end encrypted (the capsule
            never leaves the box unsealed). It carries NO credentials, and restoring
            context never restores permission &mdash; every consequential step
            re-asks with a fresh confirm, voice-id, and the master switch. The
            network transport is armed but inert.
          </div>
        </div>
      </Frame>
    </div>
  );
}

function pipelineState(h: HandoffStatus): { label: string; cls: string } {
  if (!h.enabled) return { label: "OFF", cls: "off" };
  if (!h.keyPresent) return { label: "ARMED · NEEDS PAIRING", cls: "armed" };
  return { label: "ARMED · PAIRED", cls: "ready" };
}
