import type { NotebookActivity, NotebookCite } from "../core/events";
import { notebookVerbLabel } from "../core/events";
import Frame from "./Frame";

/**
 * RESEARCH NOTEBOOKS — the read-only surface for the user's saved research
 * (daemon/src/notebook.rs, emitted as `notebook.card` from router.rs).
 *
 * It surfaces the MOST RECENT notebook voice command and what it touched:
 *   - the VERB (saved / revisited / shelf / forgotten) — the activity that ran;
 *   - the notebook's TOPIC and how many saved runs it holds;
 *   - a bounded, ALREADY-REDACTED snippet of the surfaced run's synthesis; and
 *   - the run's REAL FETCHED-SOURCE CITATIONS (a run-local id + the page title +
 *     the real URL) — the actual grounded sources the live research consulted.
 *
 * HONESTY CONTRACT (do not regress):
 *   - CITES REAL FETCHED SOURCES, NEVER FABRICATES. Every citation row is a
 *     grounded source the original SAGE run really fetched and the notebook
 *     persisted. The parser drops any citation with no url AND no title — there
 *     is nothing to point at, so none is invented. An empty list is the honest
 *     "this run had no grounded sources".
 *   - PERSIST / READ ONLY. DARWIN saves a real run that ALREADY happened and
 *     reads runs that were really saved. NO live fetch happens here, NO source
 *     is invented, and there is NO button — this panel only SHOWS the activity
 *     the daemon already produced and spoke.
 *   - HONEST EMPTY. An honest-empty revisit (a topic with no saved runs yet)
 *     carries zero runs + no citations + no snippet; the panel says so plainly
 *     rather than pretending a result.
 *   - SECRET-FREE. The wire carries only the verb, the topic, a bounded redacted
 *     snippet, the run count, and the real citation locators — never raw content,
 *     never an embedding/audio/secret.
 *
 * The reducer only ever sets `notebook` from a defensively-parsed `notebook.card`
 * whose card is non-null (a save_none/forget_none/error no-op keeps the prior
 * card, never blanks the panel) — so this component can trust the card it is
 * handed.
 */
export default function ResearchNotebooksPanel({
  activity,
}: {
  activity: NotebookActivity | null;
}) {
  // Nothing to show until a notebook command surfaces a real card. The reducer
  // holds `notebook` at null until then (and keeps the prior card on a no-op),
  // so we render nothing rather than a placeholder — mirroring the other
  // event-fed panels.
  if (activity === null || activity.card === null) return null;

  const card = activity.card;
  const isList = card.verb === "list";

  return (
    <div className="notebook-panel">
      <Frame title="RESEARCH // NOTEBOOKS" tag="REAL CITES · READ ONLY">
        <div className="notebook-body">
          <div className="notebook-head">
            <span
              className={`notebook-pill verb-${card.verb}`}
              title="what the last notebook command did — DARWIN saves a real run that already happened and reads runs really saved; nothing is fetched or invented here"
            >
              {notebookVerbLabel(card.verb)}
            </span>
            {!isList && card.topic.length > 0 && (
              <span className="notebook-topic" title="the notebook this command touched">
                {card.topic}
              </span>
            )}
            <span
              className="notebook-count"
              title={
                isList
                  ? "how many research notebooks you have saved"
                  : "how many saved runs this notebook holds — the accrued source memory"
              }
            >
              {isList
                ? `${card.runCount} ${card.runCount === 1 ? "NOTEBOOK" : "NOTEBOOKS"}`
                : `${card.runCount} ${card.runCount === 1 ? "RUN" : "RUNS"}`}
            </span>
          </div>

          {/* The honest-empty case: a revisit/shelf with no saved runs yet. */}
          {card.runCount === 0 && card.citations.length === 0 && card.snippet.length === 0 ? (
            <div className="notebook-empty dim-note">
              {isList
                ? "No research notebooks saved yet. Ask DARWIN to “save this research” after a run to start one."
                : "Nothing saved on this topic yet — no runs, no sources. DARWIN will not invent one."}
            </div>
          ) : (
            <>
              {card.snippet.length > 0 && (
                <div
                  className="notebook-snippet"
                  title="a bounded, already-redacted snippet of the surfaced run's synthesis — the full text rode the spoken reply"
                >
                  {card.snippet}
                </div>
              )}
              <CitationsSection citations={card.citations} />
            </>
          )}

          <div className="notebook-foot dim-note">
            Notebooks cite the REAL sources a research run actually fetched — the
            pages DARWIN really consulted, never invented. Saving persists a run
            that already happened; revisiting reads what was really saved.
            Forgettable, bounded, read-only.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The citations section: the run's REAL fetched-source citations, OR the honest
 *  "no grounded sources" label when the run had none. Never fabricates a row. */
function CitationsSection({ citations }: { citations: NotebookCite[] }) {
  if (citations.length === 0) {
    return (
      <div className="notebook-cites">
        <div className="notebook-cites-head">
          <span className="notebook-cites-title">SOURCES</span>
          <span
            className="notebook-pill no-sources"
            title="this run was grounded in no fetched sources — surfaced honestly rather than faked"
          >
            NO GROUNDED SOURCES
          </span>
        </div>
        <div className="notebook-empty dim-note">
          This run cited no fetched sources. That is the honest record — no
          citation is invented.
        </div>
      </div>
    );
  }

  return (
    <div className="notebook-cites">
      <div className="notebook-cites-head">
        <span className="notebook-cites-title">SOURCES</span>
        <span
          className="notebook-pill cited"
          title="the real fetched sources this run was grounded in"
        >
          {citations.length} CITED
        </span>
      </div>
      <ol className="notebook-cite-list">
        {citations.map((cite, i) => (
          <CitationRow key={`${cite.sourceId}:${cite.url}:${i}`} cite={cite} />
        ))}
      </ol>
    </div>
  );
}

/** One cited source: the page title (or the URL when there is no title) plus the
 *  real URL the run fetched. Real fetched sources only — never fabricated. */
function CitationRow({ cite }: { cite: NotebookCite }) {
  const label = cite.title.length > 0 ? cite.title : cite.url;
  return (
    <li className="notebook-cite">
      <span className="notebook-cite-title" title="the fetched source's title">
        {label}
      </span>
      {cite.url.length > 0 && (
        <span className="notebook-cite-url" title="the real source URL the run fetched">
          {cite.url}
        </span>
      )}
    </li>
  );
}
