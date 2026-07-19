import type { CodeCite, CodeExplained, CodeProposal } from "../core/events";
import { codeMethodLabel } from "../core/events";
import type { CodeIntel, CodeNote } from "../core/state";
import Frame from "./Frame";

/**
 * CODE INTEL // ON-DEVICE — the read-only / propose-only surface for the code
 * intelligence tools over the user's OWN allowlisted codebase root
 * (daemon/src/code.rs: code_explain + code_propose_diff). It mirrors two existing
 * postures: the docsearch panel (cited hits — real file + offset + snippet) for
 * EXPLANATIONS, and the forge/heal review panels (a propose-only artifact shown
 * READ-ONLY with the EXACT MANUAL apply command, NO one-click apply) for DIFFS.
 *
 * It shows, all from the local 127.0.0.1 broadcast, SECRET-FREE:
 *   - the last GROUNDED + CITED explanation: the question, the ranking method
 *     that ACTUALLY ran (neural on-device embeddings vs the lexical BM25
 *     fallback), and the REAL cited code chunks (file path + byte offset +
 *     snippet) the answer was grounded in. An empty hit set is the daemon's
 *     HONEST "not indexed" reply — shown, never hidden or faked.
 *   - a pending PROPOSE-ONLY diff: the proposal <ts>, how many real indexed
 *     chunks it was grounded in, and the EXACT MANUAL apply command
 *     (scripts/apply_code_diff.sh <ts>). The diff lives in the proposal store;
 *     your code is UNTOUCHED.
 *   - an HONEST non-error note when a draft was rejected (not a usable/confined
 *     diff) or blocked (an abort stage). "disabled" (the shipped-OFF gate) never
 *     reaches here — it is the inert default, not a failure.
 *
 * HONESTY CONTRACT (do not regress — the same posture as the heal/forge/docsearch
 * surfaces this mirrors):
 *   - GROUNDED + CITED, NEVER FABRICATES. Every cited chunk is a real indexed
 *     chunk the daemon returned (the parser drops any hit with no file to point
 *     at). An empty result is the honest "nothing indexed matched", shown — the
 *     panel never invents code that is not in the index.
 *   - PROPOSE-ONLY, NO ONE-CLICK APPLY. There is deliberately NO button that
 *     applies/runs the diff. The ONLY apply route is the human running the shown
 *     terminal command after reviewing the diff — surfacing that confined,
 *     re-validating command is the whole design (mirrors the forge/heal panels).
 *     The copy makes explicit your code is untouched until you run it.
 *   - CODE QUALITY IS NOT GUARANTEED. The model authored the diff; whether it
 *     compiles/works is runtime/model-gated and is NOT a measured claim. The
 *     apply script re-validates + writes only under your allowlisted root.
 *   - ON-DEVICE INDEX, SECRET-FREE. The code index is on-device; these events
 *     ride the local broadcast only and carry nothing but what the persona
 *     already speaks/shows (the question, real cited chunks, a <ts>, a count, a
 *     short reason) — never an embedding/secret/token.
 *   - SHIPPED OFF + ALLOWLIST-ONLY. The [code] feature is disabled by default and
 *     touches nothing until the operator enables it AND allowlists a codebase
 *     root — so this panel stays empty until then.
 *
 * The reducer only ever sets `codeIntel` from defensively-parsed code.* events
 * (real cited chunks / a ts-bearing proposal / a short reason — never a secret),
 * so this component can trust the fields it is handed.
 */
export default function CodeIntelPanel({ code }: { code: CodeIntel | null }) {
  // Nothing to show until a code.* event lands — render nothing rather than a
  // placeholder, mirroring the other event-fed panels (DocSearchPanel, McpPanel).
  // The feature ships OFF, so no event arrives until [code] is enabled AND a
  // codebase root is allowlisted.
  if (code === null) return null;
  const { explained, proposal, note } = code;
  if (explained === null && proposal === null && note === null) return null;

  return (
    <div className="codeintel-panel">
      <Frame title="CODE INTEL // ON-DEVICE" tag="GROUNDED · PROPOSE-ONLY">
        <div className="codeintel-body">
          {explained !== null && <ExplainSection explained={explained} />}
          {proposal !== null && <ProposalSection proposal={proposal} />}
          {note !== null && <NoteRow note={note} />}

          <div className="codeintel-foot dim-note">
            Explanations are GROUNDED + CITED in YOUR indexed code (the real file
            + offset chunks shown above) — nothing is invented; an empty result is
            the honest &ldquo;not indexed&rdquo;. Diffs are{" "}
            <b>PROPOSE-ONLY</b>: the change is written to the proposal store and
            your code is <b>untouched</b>. There is NO one-click apply — review the
            diff, then YOU apply it via the confined script shown, which
            re-validates and writes only under your allowlisted codebase root. The
            model authored the diff; whether it compiles/works is not guaranteed.
            The code index is on-device and ships OFF — enable{" "}
            <code>[code].enabled</code> and allowlist a root under{" "}
            <code>[code].roots</code>.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The last cited explanation: the question, the ranking method that ACTUALLY
 *  ran, and the real cited code chunks. An empty hit set is the HONEST
 *  not-indexed reply — shown plainly, never hidden or faked. */
function ExplainSection({ explained }: { explained: CodeExplained }) {
  // Neural iff a "neural-*" method (embedding OR neural-then-reranked); a
  // "lexical-*" method retrieved by BM25. The tooltip names any rerank stage.
  // Hybrid is neural-inclusive (uses the embeddings) -> neural pill.
  const neural = explained.method.startsWith("neural-") || explained.method.startsWith("hybrid");
  const explainTitle =
    explained.method === "neural-embedding"
      ? "grounded chunks were retrieved by cosine over on-device embedding vectors"
      : explained.method === "neural-reranked"
        ? "grounded chunks were retrieved by cosine over on-device embedding vectors, then re-ranked by an on-device cross-encoder"
        : explained.method === "lexical-reranked"
          ? "grounded chunks were retrieved by BM25 keyword relevance (the embedding vector space was stale/unavailable), then re-ranked by an on-device cross-encoder"
          : explained.method === "lexical-bm25"
            ? "grounded chunks were retrieved by BM25 keyword relevance (the on-device embedder was unavailable)"
            : explained.method === "hybrid"
              ? "grounded chunks were retrieved by FUSING on-device embedding cosine with BM25 keyword relevance (reciprocal rank fusion)"
              : explained.method === "hybrid-reranked"
                ? "grounded chunks were retrieved by fusing embedding cosine with BM25 (reciprocal rank fusion), then re-ranked by an on-device cross-encoder"
                : `retrieval method: ${codeMethodLabel(explained.method)}`;
  return (
    <div className="codeintel-explain">
      <div className="codeintel-head">
        <span className="codeintel-title">LAST EXPLAIN</span>
        <span
          className={`codeintel-pill ${neural ? "neural" : "bm25"}`}
          title={explainTitle}
        >
          {codeMethodLabel(explained.method)}
        </span>
      </div>

      {explained.question.length > 0 && (
        <div className="codeintel-q">
          <span className="codeintel-q-label">Q</span>
          <span className="codeintel-q-text">{explained.question}</span>
        </div>
      )}

      {explained.hits.length === 0 ? (
        <div className="codeintel-empty dim-note">
          Nothing in the code index matched — this is the HONEST result, no code is
          invented. If you have not indexed yet, enable code intelligence and
          allowlist your codebase root, then ask again.
        </div>
      ) : (
        <>
          <div className="codeintel-grounded dim-note">
            Grounded in {explained.hits.length} cited{" "}
            {explained.hits.length === 1 ? "chunk" : "chunks"} from your indexed
            code (the spoken answer cites these):
          </div>
          <div className="codeintel-hits">
            {explained.hits.map((h, i) => (
              <CiteRow key={`${h.filePath}:${h.byteOffset}:${i}`} cite={h} />
            ))}
          </div>
        </>
      )}
    </div>
  );
}

/** One cited code chunk: the real file path + byte offset (the citation anchor)
 *  and the bounded snippet the daemon already cited — a real indexed chunk, never
 *  fabricated. */
function CiteRow({ cite }: { cite: CodeCite }) {
  return (
    <div className="codeintel-hit">
      <div className="codeintel-hit-head">
        <span className="codeintel-hit-path" title={cite.filePath}>
          {cite.filePath}
        </span>
        <span
          className="codeintel-hit-offset"
          title="byte offset of the cited chunk in the file"
        >
          @{cite.byteOffset}
        </span>
      </div>
      {cite.snippet.length > 0 && (
        <pre className="codeintel-hit-snippet">{cite.snippet}</pre>
      )}
    </div>
  );
}

/** The pending PROPOSE-ONLY diff: the proposal <ts>, how many chunks it was
 *  grounded in, and the EXACT MANUAL apply command. REVIEW-ONLY — there is
 *  deliberately NO button that applies it (mirrors the forge/heal review panels);
 *  surfacing the confined, re-validating command is the whole design. */
function ProposalSection({ proposal }: { proposal: CodeProposal }) {
  // The EXACT manual apply command — the ONLY route that ever touches your code.
  const applyCmd = `scripts/apply_code_diff.sh ${proposal.ts}`;
  return (
    <div className="codeintel-proposal">
      <div className="codeintel-head">
        <span className="codeintel-title">PROPOSED DIFF</span>
        <span className="codeintel-pill propose" title="a reviewable diff in the proposal store — your code is untouched">
          PROPOSE-ONLY
        </span>
      </div>

      <div className="codeintel-prop-meta dim-note">
        A reviewable unified diff was written to the proposal store — your code is{" "}
        <b>untouched</b>. Grounded in {proposal.groundedHits} cited{" "}
        {proposal.groundedHits === 1 ? "chunk" : "chunks"} of your indexed code.
      </div>

      {/* The EXACT manual apply command — the ONLY install route. No one-click. */}
      <div className="codeintel-review">
        <div className="codeintel-review-label">
          TO APPLY (MANUAL — REVIEW THE DIFF FIRST)
        </div>
        <div className="codeintel-cmd" role="note">
          <span className="codeintel-cmd-prompt" aria-hidden="true">
            $
          </span>
          <code>{applyCmd}</code>
        </div>
      </div>

      <div className="codeintel-safety dim-note">
        Nothing is applied yet. Review the diff under{" "}
        <code>state/code/proposals/{proposal.ts}/</code>, then run the command
        above — it re-validates the diff and writes only under your allowlisted
        codebase root (confined by construction). There is no auto-apply, and the
        model&rsquo;s code quality is not guaranteed.
      </div>
    </div>
  );
}

/** An HONEST non-error note: the model's draft was rejected (not a usable/confined
 *  diff) or the tool was blocked (an abort stage). REVIEW-ONLY attention (NOT the
 *  red alert chrome) — the propose-only contract held; nothing was changed. */
function NoteRow({ note }: { note: CodeNote }) {
  const label = note.kind === "rejected" ? "DRAFT REJECTED" : "BLOCKED";
  const lead =
    note.kind === "rejected"
      ? "The model's draft was not a usable, confined diff — nothing was proposed and nothing changed."
      : "The tool did not complete — nothing was proposed and nothing changed.";
  return (
    <div className="codeintel-note">
      <div className="codeintel-head">
        <span className="codeintel-title">{label}</span>
        <span className="codeintel-pill note" title="an honest non-error result — nothing was changed">
          NO CHANGE
        </span>
      </div>
      <div className="codeintel-note-body dim-note">
        {lead} <span className="codeintel-note-reason">({note.detail})</span>
      </div>
    </div>
  );
}
