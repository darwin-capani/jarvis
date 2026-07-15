import type { ArtifactPeek, ArtifactCitation } from "../core/events";
import { isKnownArtifactKind } from "../core/events";
import Frame from "./Frame";

/**
 * ARTIFACT QUICKLOOK OVERLAY (artifact.rs) — the single overlay that renders ANY
 * artifact the assistant produced, read back out of the daemon's bounded, on-device
 * Artifact Registry (emitted as `artifact.peek` from the peek voice op / the
 * `artifact_peek` tool, summoned by "what did you just do" / "peek").
 *
 * It surfaces the MOST RECENT (or an id'd) artifact:
 *   - a KIND-AWARE header (report / chart / code-diff / draft / notebook / forecast
 *     / docsearch / image, or a generic fallback for an unknown kind);
 *   - the artifact TITLE + a compact, redacted PREVIEW line (the producer's own);
 *   - a PROVENANCE FOOTER: the REAL producing agent + the REAL citations — or a
 *     plain UNCITED verdict when the artifact carried no source.
 *
 * HONESTY CONTRACT (LOAD-BEARING — Share Guard will ride on this overlay):
 *   - REAL PROVENANCE. The agent + citations are the daemon's real values; the
 *     overlay never invents an agent or a source.
 *   - UNCITED IS UNCITED. `artifact.uncited` is re-derived by the parser from the
 *     surviving citations, so an artifact with no real source shows the honest
 *     UNCITED pill (amber — an honest state, never the reserved alert red) and is
 *     never dressed up with a fabricated citation.
 *   - BOUNDED, REVIEW-ONLY, SECRET-FREE. It SHOWS what was already produced — no
 *     button, no action, no network. The wire carries only the kind, title,
 *     preview, agent, and citation locators, never a raw body.
 *
 * The reducer only ever sets `artifactPeek` from a defensively-parsed `artifact.peek`
 * that carried a usable kind, so this component can trust the artifact it is handed.
 */

/** A human label per known kind (the closed vocabulary). An unknown kind renders
 *  generically (never guessed into a richer kind). */
const KIND_LABEL: Record<string, string> = {
  report: "REPORT",
  chart: "CHART",
  image: "IMAGE",
  draft: "DRAFT",
  code_diff: "CODE DIFF",
  notebook: "NOTEBOOK",
  forecast: "FORECAST",
  docsearch: "DOC SEARCH",
};

/** The kind label the header shows — a known kind's friendly name, or the raw wire
 *  kind (uppercased) for an unrecognized one, rendered as a generic artifact. */
function kindLabel(kind: string): string {
  if (isKnownArtifactKind(kind)) return KIND_LABEL[kind];
  const trimmed = kind.trim();
  return trimmed.length > 0 ? trimmed.toUpperCase() : "ARTIFACT";
}

export default function QuickLookOverlay({ artifact }: { artifact: ArtifactPeek | null }) {
  // Nothing to show until an artifact.peek arrives. The registry ships ARMED but is
  // empty until a producer registers something AND a peek is summoned — render
  // nothing rather than a placeholder (mirrors the other event-fed panels).
  if (artifact === null) return null;

  const label = kindLabel(artifact.kind);
  const title = artifact.title.length > 0 ? artifact.title : "(untitled)";

  return (
    <div className="quicklook-overlay">
      <Frame title={`QUICKLOOK // ${label}`} tag="PEEK · REVIEW ONLY">
        <div className="quicklook-body">
          <div className="quicklook-head">
            <span className="quicklook-kind" title="what was produced">
              {label}
            </span>
            <span className="quicklook-title" title="the artifact title">
              {title}
            </span>
            {artifact.uncited ? (
              <span
                className="quicklook-pill uncited"
                title="the artifact carried no source — shown as uncited, never dressed up"
              >
                UNCITED
              </span>
            ) : (
              <span
                className="quicklook-pill cited"
                title="the real sources the artifact rests on — never fabricated"
              >
                {artifact.citationCount} CITED
              </span>
            )}
          </div>

          {artifact.preview.length > 0 && (
            <div className="quicklook-preview" title="a compact, redacted preview of the artifact">
              {artifact.preview}
            </div>
          )}

          <ProvenanceFooter artifact={artifact} />

          <div className="quicklook-foot dim-note">
            The last thing I produced, read back out of the on-device registry. The
            provenance is honest — the real producing agent and the real sources, or
            plainly UNCITED when it carried none (never a fabricated source). This is a
            review-only peek — it shows what was already made, takes no action, and
            reaches no network. Armed by default behind <code>[artifact].enabled</code>.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The PROVENANCE FOOTER — the REAL producing agent + the REAL citations. When the
 *  artifact is uncited it says so plainly (never implies a source it lacks). */
function ProvenanceFooter({ artifact }: { artifact: ArtifactPeek }) {
  const agent = artifact.agent.length > 0 ? artifact.agent : "unknown agent";
  return (
    <div className="quicklook-provenance">
      <div className="quicklook-provenance-head">
        <span className="quicklook-provenance-title">PROVENANCE</span>
        <span className="quicklook-agent" title="the real agent that produced this">
          by {agent}
        </span>
      </div>
      {artifact.uncited ? (
        <div className="quicklook-uncited dim-note">
          Uncited — this artifact carried no source. I&rsquo;m showing it as uncited
          rather than dressing it up with a citation it doesn&rsquo;t have.
        </div>
      ) : (
        <CitationsList citations={artifact.citations} total={artifact.citationCount} />
      )}
    </div>
  );
}

/** The citations list — the REAL source locators the artifact rests on. Each row is
 *  a real citation the producing path carried (title + locator); the parser dropped
 *  any with no usable locator. The count is the daemon's real total, which the
 *  bounded list may be shorter than. */
function CitationsList({
  citations,
  total,
}: {
  citations: ArtifactCitation[];
  total: number;
}) {
  if (citations.length === 0) {
    // Cited total > 0 but none survived the preview bound — surface the honest
    // count rather than implying there are none.
    return (
      <div className="quicklook-citations">
        <span className="quicklook-more dim-note">
          {total} {total === 1 ? "source" : "sources"} back this artifact
        </span>
      </div>
    );
  }
  const more = total - citations.length;
  return (
    <div className="quicklook-citations">
      <ul className="quicklook-citation-list">
        {citations.map((c, i) => (
          <li className="quicklook-citation" key={`${c.title}:${c.url}:${i}`}>
            {c.title.length > 0 && (
              <span className="quicklook-citation-title">{c.title}</span>
            )}
            {c.url.length > 0 && (
              <span className="quicklook-citation-url" title="the real source locator">
                {c.url}
              </span>
            )}
          </li>
        ))}
      </ul>
      {more > 0 && (
        <div className="quicklook-more dim-note">
          + {more} more {more === 1 ? "source" : "sources"}
        </div>
      )}
    </div>
  );
}
