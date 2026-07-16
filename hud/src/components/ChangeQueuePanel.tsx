import type { ChangeqState, PendingChange } from "../core/changeq";
import { hasPending, kindLabel, mirroredCount } from "../core/changeq";
import Frame from "./Frame";

/**
 * CHANGE QUEUE // REVIEW LANE — the read-only surface for the unified git-native
 * review lane over every PROPOSE-ONLY artifact DARWIN produces (daemon/src/
 * changeq.rs: self-heal patches, code diffs, forged apps, routing optimizations).
 * It mirrors the posture of the heal/forge/code review panels: a propose-only
 * artifact shown READ-ONLY with the EXACT MANUAL apply command — there is NO
 * one-click apply.
 *
 * It shows, all from the local 127.0.0.1 broadcast, SECRET-FREE:
 *   - each PENDING proposal: its kind (self-heal / code / forge / optimize), its
 *     proposal <ts>, the confined artifact locator (the proposal-store path under
 *     state/), a short summary, its provenance (the agent + model that produced
 *     it + a content fingerprint — NEVER a secret/token), and whether it has been
 *     mirrored onto the dedicated local review branch (darwin/changeq);
 *   - the EXACT existing apply command for each (scripts/apply_*.sh <ts>) — the
 *     SAME human-gated, re-validating script that proposal type has always used.
 *
 * HONESTY CONTRACT (do not regress — the same posture as the heal/forge/code
 * surfaces this mirrors):
 *   - PROPOSE-ONLY, NO ONE-CLICK APPLY. There is deliberately NO button that
 *     applies a proposal. The ONLY apply route is the human running the shown
 *     command, which re-validates and applies under THAT type's own gate. The
 *     change queue invents NO new apply authority.
 *   - SECRET-FREE. The frame carries only kinds, ts, confined locators, summaries,
 *     apply commands, and sanitized provenance — never a diff body or a token.
 *   - GIT-NATIVE REVIEW + ROLLBACK. Each proposal is mirrored onto a dedicated
 *     LOCAL branch for review; rollback is a safe `git revert` (never a reset).
 */
export default function ChangeQueuePanel({ changeq }: { changeq: ChangeqState | null }) {
  // Nothing to show until a changeq.list frame with pending proposals lands —
  // render nothing rather than a placeholder, mirroring the other event-fed
  // panels (CodeIntelPanel, ForgePanel). `hasPending` is the pure, unit-tested gate.
  if (!hasPending(changeq)) return null;

  const mirrored = mirroredCount(changeq);
  const total = changeq.pending.length;

  return (
    <div className="changeq-panel">
      <Frame title="CHANGE QUEUE // REVIEW LANE" tag="PROPOSE-ONLY · GIT-NATIVE">
        <div className="changeq-body">
          <div className="changeq-head dim-note">
            {total} pending proposal{total === 1 ? "" : "s"} · {mirrored}/{total} mirrored onto{" "}
            <code>{changeq.branch}</code>
          </div>

          <div className="changeq-list">
            {changeq.pending.map((c) => (
              <ProposalRow key={`${c.kind}:${c.ts}`} change={c} />
            ))}
          </div>

          <div className="changeq-foot dim-note">
            Every entry is <b>PROPOSE-ONLY</b> — nothing here is applied. There is NO
            one-click apply: review a proposal, then YOU run its apply command, which
            re-validates and applies under that type&rsquo;s own gate. Each proposal is
            mirrored onto the local <code>{changeq.branch}</code> review branch;
            rollback is a safe <code>git revert</code>. This lane invents no new apply
            authority — it routes to each writer&rsquo;s existing gated script.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One pending proposal: kind, ts, summary, provenance, mirror status, and the
 *  EXACT manual apply command (no one-click apply). */
function ProposalRow({ change }: { change: PendingChange }) {
  const { kind, ts, artifact, summary, applyCommand, committed, provenance } = change;
  return (
    <div className={`changeq-row changeq-kind-${kind}`}>
      <div className="changeq-row-head">
        <span className={`changeq-pill changeq-pill-${kind}`}>{kindLabel(kind)}</span>
        <span className="changeq-ts" title="proposal timestamp">
          #{ts}
        </span>
        <span
          className={`changeq-status ${committed ? "mirrored" : "queued"}`}
          title={
            committed
              ? "mirrored onto the review branch for git-native review + rollback"
              : "queued — not yet mirrored onto the review branch (no repo yet, or pending the next sweep)"
          }
        >
          {committed ? "MIRRORED" : "QUEUED"}
        </span>
      </div>

      {summary.length > 0 && <div className="changeq-summary">{summary}</div>}

      <div className="changeq-artifact dim-note">
        <span className="changeq-artifact-label">artifact</span>{" "}
        <code>{artifact}</code>
      </div>

      <div className="changeq-prov dim-note">
        by <b>{provenance.agent}</b> · model <code>{provenance.model}</code> · run{" "}
        <code>{provenance.run}</code> · state <code>{provenance.stateHash}</code>
      </div>

      <div className="changeq-apply">
        <span className="changeq-apply-label dim-note">to apply (human-gated, re-validates)</span>
        <code className="changeq-apply-cmd">{applyCommand}</code>
      </div>
    </div>
  );
}
