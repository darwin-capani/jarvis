import type { MirrorBelief, MirrorFrame } from "../core/events";
import Frame from "./Frame";

/**
 * MIRROR // SELF-MODEL — belief-audit + contest over what DARWIN believes about
 * the user (daemon user_model.rs -> `mirror.belief`). Each row is one stored belief
 * with an EVIDENCE chip (how many times it was observed + how many real provenance
 * sources) and a "that's wrong" control that CONTESTS it.
 *
 * HONESTY CONTRACT (do not regress):
 *   - REVIEW-ONLY here: the panel shows only what the daemon ALREADY observed —
 *     the stored observation, observed-count, and provenance. Nothing is fabricated;
 *     an empty profile says so plainly rather than inventing a belief.
 *   - REDUCE-ONLY contest: "that's wrong" only ever REMOVES a belief. It routes
 *     through the EXISTING gated `ask` command channel (the daemon drops the belief
 *     AND writes a suppression tombstone it never re-derives past) — the panel never
 *     writes the model directly, and can never touch a private agent.* note.
 *   - SECRET-FREE: every field is the user's OWN already-redacted profile.
 */
export default function MirrorPanel({
  mirror,
  onContest,
}: {
  mirror: MirrorFrame | null;
  onContest: (belief: MirrorBelief) => void;
}) {
  if (mirror === null) return null;

  const suppressedCount = mirror.suppressed.length;

  return (
    <div className="mirror-panel">
      <Frame title="MIRROR // SELF-MODEL" tag="REVIEW ONLY">
        <div className="mirror-body">
          <div className="mirror-head dim-note">
            What I have OBSERVED about you — say “why do you think…” to hear the
            evidence, or contest anything that is wrong.
            {suppressedCount > 0 && (
              <span className="mirror-suppressed-count">
                {" "}
                · {suppressedCount} contested &amp; suppressed
              </span>
            )}
          </div>
          {mirror.beliefs.length === 0 ? (
            <div className="mirror-empty dim-note">
              I have not built up an observed picture of you yet — nothing has met the
              bar to record. I only note what I actually observe, never guess.
            </div>
          ) : (
            <ul className="mirror-beliefs">
              {mirror.beliefs.map((b) => (
                <BeliefRow key={b.key} belief={b} onContest={onContest} />
              ))}
            </ul>
          )}
          <div className="mirror-foot dim-note">
            Review only — every line is a real observation with its evidence.
            “That’s wrong” drops the belief and stops me re-deriving it; nothing here
            is consequential.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One belief: the facet label, the observation, an evidence chip, and the contest
 *  control. */
function BeliefRow({
  belief,
  onContest,
}: {
  belief: MirrorBelief;
  onContest: (belief: MirrorBelief) => void;
}) {
  const sources = belief.provenance.length;
  return (
    <li className={`mirror-belief ${belief.facet}`}>
      <div className="mirror-belief-main">
        <span className="mirror-facet">{facetLabel(belief.facet)}</span>
        <span className="mirror-observation">{belief.observation}</span>
      </div>
      <div className="mirror-belief-meta">
        <span className="mirror-evidence-chip" title="the observed-count and how many real sources back this belief">
          observed {belief.observedCount}×
          {sources > 0 ? ` · ${sources} source${sources === 1 ? "" : "s"}` : ""}
        </span>
        <button
          type="button"
          className="mirror-contest"
          onClick={() => onContest(belief)}
          title="Contest this belief — I will drop it and stop re-deriving it"
          aria-label={`Contest belief: ${belief.observation}`}
        >
          that’s wrong
        </button>
      </div>
    </li>
  );
}

/** Map a facet token (the daemon's `Facet::as_str`) to a human label; an unknown
 *  token is shown verbatim rather than dropped (the daemon owns the vocabulary). */
export function facetLabel(facet: string): string {
  switch (facet) {
    case "preference":
      return "Preference";
    case "pattern":
      return "Pattern";
    case "topic":
      return "Recurring topic";
    case "style":
      return "Communication style";
    default:
      return facet;
  }
}
