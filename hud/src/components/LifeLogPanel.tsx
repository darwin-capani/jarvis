import type { LifeLogDigest } from "../core/events";
import { lifeLogPeriodLabel } from "../core/events";
import Frame from "./Frame";

/**
 * LIFE-LOG — the read-only surface for the user's own activity digest
 * (daemon/src/lifelog.rs, emitted as `lifelog.digest` from router.rs).
 *
 * It surfaces the MOST RECENT life-log voice command and its digest:
 *   - the PERIOD (today / this week) and the REAL recorded-episode count;
 *   - the rendered DIGEST TEXT (the same honest, empty-aware line DARWIN spoke);
 *   - the bounded, ALREADY-REDACTED salient THEMES and distinct TOPICS; and
 *   - a bounded sample of the most-recent already-redacted SUMMARIES.
 *
 * HONESTY CONTRACT (do not regress):
 *   - SUMMARIZES REAL EPISODES, NEVER INVENTS. Every theme/topic/summary derives
 *     from the user's REAL recorded turns (the agent-scoped episodic store),
 *     already redacted before write. An empty/sparse window is shown as such —
 *     no event is fabricated to fill it.
 *   - HONEST EMPTY. When the window held no episodes the digest rides
 *     `empty: true` with a zero count and empty lists; the panel renders the
 *     plain "nothing logged" state rather than a placeholder activity.
 *   - BOUNDED + FORGETTABLE. The lists are capped (a glance, not a dump); the
 *     underlying episodes are the user's own and forgettable. READ-ONLY: there
 *     is NO button — this panel only SHOWS the digest the daemon produced.
 *   - SECRET-FREE. Every field is the episodic store's already-redacted output
 *     (a secret was stripped before write) — never a raw episode, never an
 *     embedding/audio/secret.
 *
 * The reducer only ever sets `lifelog` from a defensively-parsed `lifelog.digest`
 * with a recognized period (an unparseable one is dropped) — so this component
 * can trust the digest it is handed.
 */
export default function LifeLogPanel({ digest }: { digest: LifeLogDigest | null }) {
  // Nothing to show until a life-log command runs. The reducer holds `lifelog`
  // at null until the first digest arrives — render nothing rather than a
  // placeholder, mirroring the other event-fed panels.
  if (digest === null) return null;

  return (
    <div className="lifelog-panel">
      <Frame title="LIFE-LOG // DIGEST" tag="YOUR EPISODES · READ ONLY">
        <div className="lifelog-body">
          <div className="lifelog-head">
            <span className="lifelog-pill period" title="the window this digest covers">
              {lifeLogPeriodLabel(digest.period)}
            </span>
            <span
              className="lifelog-count"
              title="the REAL number of recorded turns in this window — never padded"
            >
              {digest.episodeCount} {digest.episodeCount === 1 ? "TURN" : "TURNS"}
            </span>
          </div>

          {/* The rendered digest line — the same honest, empty-aware text DARWIN
              spoke. */}
          {digest.digestText.length > 0 && (
            <div className="lifelog-digest-text">{digest.digestText}</div>
          )}

          {digest.empty ? (
            <div className="lifelog-empty dim-note">
              Nothing logged for {digest.period}. DARWIN will not invent an event
              to fill the gap — an empty window stays empty.
            </div>
          ) : (
            <>
              <TagSection label="THEMES" hint="the salient themes across your recorded turns" tags={digest.themes} />
              <TagSection label="TOPICS" hint="the distinct topics your turns touched" tags={digest.topics} />
              <SummariesSection summaries={digest.recentSummaries} />
            </>
          )}

          <div className="lifelog-foot dim-note">
            This summarizes YOUR real recorded turns — already redacted, bounded,
            and forgettable. It never invents an event; an empty window is shown
            as empty. Read-only.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** A row of bounded, already-redacted tags (themes or topics). Renders nothing
 *  when the list is empty — no empty shell. */
function TagSection({
  label,
  hint,
  tags,
}: {
  label: string;
  hint: string;
  tags: string[];
}) {
  if (tags.length === 0) return null;
  return (
    <div className="lifelog-tags">
      <span className="lifelog-tags-head" title={hint}>
        {label}
      </span>
      <div className="lifelog-tag-row">
        {tags.map((tag, i) => (
          <span className="lifelog-tag" key={`${label}:${tag}:${i}`}>
            {tag}
          </span>
        ))}
      </div>
    </div>
  );
}

/** The most-recent already-redacted summaries (each char-capped by the daemon).
 *  Renders nothing when there are none. */
function SummariesSection({ summaries }: { summaries: string[] }) {
  if (summaries.length === 0) return null;
  return (
    <div className="lifelog-summaries">
      <span
        className="lifelog-summaries-head"
        title="a bounded sample of your most-recent recorded turns, already redacted"
      >
        RECENT
      </span>
      <ul className="lifelog-summary-list">
        {summaries.map((summary, i) => (
          <li className="lifelog-summary" key={`${i}:${summary}`}>
            {summary}
          </li>
        ))}
      </ul>
    </div>
  );
}
