import type { AnswerAnnotation, AnswerSourceCite } from "../core/events";
import { confidenceLabel } from "../core/events";
import Frame from "./Frame";

/**
 * ANSWER PROVENANCE — the read-only surface for the last answer's HONEST
 * provenance (daemon/src/anthropic.rs `answers` module, emitted as
 * `answer.annotated` from main.rs run_pipeline).
 *
 * It surfaces, on the most recent answer:
 *   - the REAL SOURCES that informed it — the actual tool-result citations
 *     (a real tool name + a real locator like "indexed files" / "past episodes"
 *     / a URL + a bounded real tool-output snippet) that fed the turn; OR
 *   - the honest "FROM MY OWN KNOWLEDGE" label when the turn used NO retrieval
 *     (no source was consulted) — NEVER a fabricated citation; AND
 *   - the model's self-reported CONFIDENCE (grounded / inferred / uncertain +
 *     a one-line why).
 *
 * HONESTY CONTRACT (do not regress):
 *   - CITES REAL TOOL RESULTS, NEVER FABRICATES. Every source row is a real
 *     tool result the daemon recorded this turn (docsearch/unified/recall/
 *     episodic/web/integration reads). The parser drops any source with no tool
 *     name or no real locator — there is nothing here to invent a citation from.
 *   - "FROM MY OWN KNOWLEDGE" = NO RETRIEVAL. When the turn consulted nothing,
 *     the panel says so plainly instead of pretending a source — the honest
 *     alternative to a citation, never a fake one.
 *   - CONFIDENCE IS A SELF-REPORT, NOT A MEASUREMENT. The level + reason are the
 *     MODEL'S OWN statement (a gated prompt asks for it). The plumbing is real;
 *     the calibration is runtime/model-behavior and is NOT a measured score. The
 *     copy says so — never a "% accuracy" or a guarantee.
 *   - SHIPPED OFF. The [answers].cite / [answers].confidence gates ship false, so
 *     until they are deliberately enabled the daemon emits an EMPTY annotation
 *     (no sources, no from-my-knowledge label, no confidence) and this panel
 *     renders NOTHING — behavior is byte-for-byte today's.
 *   - SECRET-FREE. The wire carries only the real locators/snippets the persona
 *     already shows + the parsed self-report — never an embedding/audio/secret.
 *   - REVIEW-ONLY. There is NO button here. Citing/confidence are gated daemon
 *     config; this panel only SHOWS the provenance the daemon already produced.
 *
 * The reducer only ever sets `answerAnnotation` from a defensively-parsed
 * `answer.annotated` (real sources + the honest flags + the self-report, never a
 * secret) and clears it to null on an empty (off-gate) turn — so this component
 * can trust the fields it is handed, and an empty annotation never reaches it.
 */
export default function AnswerSourcesPanel({
  annotation,
}: {
  annotation: AnswerAnnotation | null;
}) {
  // Nothing to show until an answer.annotated carries real provenance. The
  // [answers] gates ship OFF, so the reducer holds `answerAnnotation` at null
  // until cite/confidence is enabled AND a real annotation arrives — render
  // nothing rather than a placeholder, mirroring the other event-fed panels
  // (DocSearchPanel, UnifiedSearchPanel, McpPanel).
  if (annotation === null) return null;

  return (
    <div className="answer-panel">
      <Frame title="ANSWER // PROVENANCE" tag="HONEST · REVIEW ONLY">
        <div className="answer-body">
          {annotation.confidence !== null && (
            <ConfidenceRow
              level={annotation.confidence.level}
              reason={annotation.confidence.reason}
            />
          )}

          {annotation.citeOn && (
            <SourcesSection
              fromMyKnowledge={annotation.fromMyKnowledge}
              sources={annotation.sources}
            />
          )}

          <div className="answer-foot dim-note">
            Citations are the REAL tool-result sources that informed the answer —
            the files, memories, episodes, or pages DARWIN actually consulted this
            turn, never invented. <b>From my own knowledge</b> means the turn used
            no retrieval (no source was consulted). Confidence is the model&rsquo;s
            own self-report (grounded / inferred / uncertain), not a measured
            accuracy score. Both ship OFF and are enabled only via{" "}
            <code>[answers].cite</code> / <code>[answers].confidence</code>.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The confidence chip + reason: the model's OWN self-report (grounded /
 *  inferred / uncertain), honestly framed — a self-report, never a measured
 *  score. */
function ConfidenceRow({
  level,
  reason,
}: {
  level: "grounded" | "inferred" | "uncertain";
  reason: string;
}) {
  return (
    <div className="answer-confidence">
      <span className="answer-confidence-head">CONFIDENCE</span>
      <span
        className={`answer-pill conf-${level}`}
        title="the model's own self-report (grounded = backed by sources it consulted; inferred = reasoned from general knowledge; uncertain = not sure) — a self-report, NOT a measured accuracy score"
      >
        {confidenceLabel(level)}
      </span>
      {reason.length > 0 && <span className="answer-confidence-reason">{reason}</span>}
    </div>
  );
}

/** The sources section: the real cited tool-result sources, OR the honest "from
 *  my own knowledge" label when the turn used no retrieval. Shown only when cite
 *  is on (the reducer already guarantees the annotation is non-empty). */
function SourcesSection({
  fromMyKnowledge,
  sources,
}: {
  fromMyKnowledge: boolean;
  sources: AnswerSourceCite[];
}) {
  if (fromMyKnowledge || sources.length === 0) {
    return (
      <div className="answer-sources">
        <div className="answer-sources-head">
          <span className="answer-sources-title">SOURCES</span>
          <span
            className="answer-pill own-knowledge"
            title="this turn consulted no retrieval source — the answer is from the model's own knowledge, honestly labeled rather than falsely cited"
          >
            FROM MY OWN KNOWLEDGE
          </span>
        </div>
        <div className="answer-empty dim-note">
          No source was consulted this turn — the answer is from the model&rsquo;s
          own knowledge. This is the honest result; no citation is invented.
        </div>
      </div>
    );
  }

  return (
    <div className="answer-sources">
      <div className="answer-sources-head">
        <span className="answer-sources-title">SOURCES</span>
        <span
          className="answer-pill cited"
          title="the real tool-result sources that actually fed this answer"
        >
          {sources.length} CITED
        </span>
      </div>
      <div className="answer-source-list">
        {sources.map((src, i) => (
          <SourceRow key={`${src.source}:${src.citation}:${i}`} src={src} />
        ))}
      </div>
    </div>
  );
}

/** One cited source: the real tool name + the real locator (the citation
 *  anchor) and the bounded snippet the daemon already cited. Real tool results
 *  only — never a fabricated citation, never a secret. */
function SourceRow({ src }: { src: AnswerSourceCite }) {
  return (
    <div className="answer-source">
      <div className="answer-source-head">
        <span className="answer-source-tool" title="the tool whose result fed the answer">
          {src.source}
        </span>
        <span className="answer-source-cite" title="the real source locator">
          {src.citation}
        </span>
      </div>
      {src.snippet.length > 0 && (
        <div className="answer-source-snippet">{src.snippet}</div>
      )}
    </div>
  );
}
