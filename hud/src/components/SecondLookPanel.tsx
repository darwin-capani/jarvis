import type { ConsensusAdvisory } from "../core/events";
import Frame from "./Frame";

/**
 * SECOND LOOK // PRE-CONFIRM — the adversarial advisory for the currently
 * PARKED consequential action (daemon consensus.rs -> `consensus.advisory`).
 * The same notes are spoken with the confirmation prompt; this panel shows
 * them verbatim (the spoken path is model-mediated and may paraphrase).
 *
 * HONESTY CONTRACT (do not regress):
 *   - ADVISORY ONLY. Nothing here approves, denies, or executes anything —
 *     the spoken-confirm gate is byte-for-byte unchanged, and the panel says
 *     so in its standing footnote.
 *   - Notes are zero-fabrication: reversibility from the undo journal's own
 *     derivation, first-time-recipient from the bounded recent record
 *     (honestly worded — "not in my recent record", never "never contacted"),
 *     and targeted risk flags. Redacted daemon-side before the wire.
 *   - The panel clears when the pending action resolves or is superseded —
 *     it never describes an action that is no longer awaiting the user.
 */
export default function SecondLookPanel({ advisory }: { advisory: ConsensusAdvisory | null }) {
  if (advisory === null) return null;

  return (
    <div className="secondlook-panel">
      <Frame title="SECOND LOOK // PRE-CONFIRM" tag="ADVISORY">
        <div className="secondlook-body">
          <div className="secondlook-head">
            <span className="secondlook-pill">AWAITING YOUR CONFIRM</span>
            <span className="secondlook-tool">{advisory.tool}</span>
          </div>
          <ul className="secondlook-notes">
            {advisory.notes.map((n, i) => (
              <li key={`${n.slice(0, 40)}-${i}`} className="secondlook-note">
                {n}
              </li>
            ))}
          </ul>
          <div className="secondlook-foot dim-note">
            Advisory only — the gate is unchanged. Nothing runs without your
            spoken confirm.
          </div>
        </div>
      </Frame>
    </div>
  );
}
