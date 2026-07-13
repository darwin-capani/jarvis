import type { ProvenanceClaim, ResearchProvenance } from "../core/events";
import Frame from "./Frame";

/**
 * RESEARCH // PROVENANCE LEDGER — the per-run grounding split (daemon
 * research.rs -> `research.provenance`). One row-group per completed research
 * run: how many claims were grounded in actually-fetched sources, and — the
 * part no other surface shows — WHICH claims were SET ASIDE as unsourced (the
 * spoken answer reduces them to a bare count).
 *
 * HONESTY CONTRACT (do not regress):
 *   - A run with every claim grounded gets the green ALL GROUNDED pill; ANY
 *     set-aside claim turns the run's pill amber with the honest fraction.
 *   - Set-aside claims are listed first and labelled — never silently blended
 *     into the grounded list.
 *   - `truncated` runs say so: the bibliography the counts describe was
 *     bounded, and the ledger must not present a bounded run as exhaustive.
 *   - The ledger is a bounded newest-first ring; a fresh HUD honestly shows
 *     nothing until a research run completes.
 */
export default function ProvenanceLedgerPanel({ runs }: { runs: ResearchProvenance[] }) {
  if (runs.length === 0) return null;

  return (
    <div className="provenance-panel">
      <Frame title="RESEARCH // PROVENANCE LEDGER" tag="SAGE">
        <div className="provenance-body">
          {runs.map((run, i) => (
            <RunGroup key={`${run.question}-${i}`} run={run} />
          ))}
          <div className="provenance-foot dim-note">
            Claims map only to sources that were actually fetched; anything
            unsourced is flagged as set aside — never presented as fact.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One research run: question, grounding pill, and the claim rows. */
function RunGroup({ run }: { run: ResearchProvenance }) {
  const clean = run.claimsUngrounded === 0;
  return (
    <div className="provenance-run">
      <div className="provenance-head">
        <span className={`provenance-pill ${clean ? "clean" : "flagged"}`}>
          {clean
            ? "ALL GROUNDED"
            : `${run.claimsGrounded}/${run.claimsTotal} GROUNDED`}
        </span>
        <span className="provenance-question">{run.question}</span>
      </div>
      <div className="provenance-meta dim-note">
        {run.sourcesFetched} source{run.sourcesFetched === 1 ? "" : "s"} fetched
        {run.truncated ? " · run truncated by its budget — not exhaustive" : ""}
        {run.claimsOmitted > 0
          ? ` · ${run.claimsOmitted} more claim${run.claimsOmitted === 1 ? "" : "s"} not shown`
          : ""}
      </div>
      <ul className="provenance-claims">
        {run.claims.map((c, i) => (
          <ClaimRow key={`${c.text.slice(0, 40)}-${i}`} claim={c} />
        ))}
      </ul>
    </div>
  );
}

/** One claim: set-aside rows lead with the honest flag; grounded rows carry
 *  their real backing source. */
function ClaimRow({ claim }: { claim: ProvenanceClaim }) {
  return (
    <li className={`provenance-claim ${claim.grounded ? "grounded" : "set-aside"}`}>
      {!claim.grounded && <span className="provenance-flag">SET ASIDE · UNSOURCED</span>}
      <span className="provenance-text">{claim.text}</span>
      {claim.grounded && (
        <span className="provenance-source dim-note">
          [{claim.sourceId}] {claim.sourceTitle}
        </span>
      )}
    </li>
  );
}
