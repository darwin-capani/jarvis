import type { DocHit, DocIndexStatus, DocSearchResult } from "../core/events";
import Frame from "./Frame";

/**
 * FILE SEARCH // ON-DEVICE — the read-only surface for the on-device file RAG
 * (daemon/src/docsearch.rs). It shows the INDEX STATUS (how many of the user's
 * own allowlisted files/chunks are indexed and whether search runs neural or
 * BM25) and the last CITED search result (real indexed file path + offset +
 * snippet + the method that actually ran).
 *
 * HONESTY CONTRACT (do not regress):
 *   - 100% ON-DEVICE / PRIVATE. File contents + embeddings NEVER leave the
 *     device — the embed op is the on-device MLX model and these events ride the
 *     local 127.0.0.1 telemetry broadcast only. The footer says so plainly.
 *   - SHIPPED OFF + ALLOWLIST-ONLY. The feature is disabled by default and
 *     indexes NOTHING until the operator flips [docsearch].enabled AND allowlists
 *     a folder — never a whole-disk scan. Until an index is built this panel
 *     shows the honest "not indexed yet" state, not a fake.
 *   - CITES REAL FILES, NEVER FABRICATES. Every hit is a real indexed chunk the
 *     daemon returned (the parser drops any hit with no file to point at). An
 *     empty result is the honest "nothing found", shown — never hidden.
 *   - HONEST METHOD. The result names the backend that ACTUALLY ran — neural
 *     on-device embeddings, or lexical BM25 when the on-device embedder was down.
 *     The index status pill warns when some chunks are not embedded (search over
 *     them falls back to BM25).
 *   - HONEST EXTRACTORS + GUARD. Text-like files, born-digital PDFs and Office
 *     docs are extracted on-device; a scanned/encrypted/corrupt file is SKIPPED
 *     honestly, never guessed at. PDF decoding runs inside the daemon's
 *     memory-jailed pdfjail subprocess when the helper is present — the guard
 *     pill reports which guard is ACTUALLY active (`docsearch.status`), amber
 *     when the daemon is silently on the weaker in-process fallback.
 *   - REVIEW-ONLY. There is NO button here that indexes, searches, or clears the
 *     index. Indexing/searching/forgetting are SPOKEN intents (the HUD never
 *     triggers a disk read or writes daemon config); this panel only SHOWS the
 *     state and the commands, mirroring the MCP / MEMORY surfaces.
 *
 * The reducer only ever sets `docIndex` from a defensively-parsed
 * `docsearch.indexed` (counts only, never a path), `docSearch` from a parsed
 * `docsearch.searched` (only real returned hits, with the honest method), and
 * `pdfJail` from a STRICT `docsearch.status` parse (only a literal `true`
 * claims the jail is armed), so this component can trust the fields it is handed.
 */
export default function DocSearchPanel({
  index,
  search,
  pdfJail,
}: {
  index: DocIndexStatus | null;
  search: DocSearchResult | null;
  pdfJail: boolean | null;
}) {
  // Nothing to show until the user has either built an index or run a search —
  // render nothing rather than a placeholder, mirroring the other event-fed
  // panels (McpPanel, EvalPanel). The feature ships OFF, so neither event arrives
  // until it is deliberately enabled + a folder allowlisted + indexed/searched.
  if (index === null && search === null) return null;

  return (
    <div className="docsearch-panel">
      <Frame title="FILE SEARCH // ON-DEVICE" tag="PRIVATE · REVIEW ONLY">
        <div className="docsearch-body">
          <IndexStatusRow index={index} pdfJail={pdfJail} />
          <SearchResults search={search} />

          <div className="docsearch-foot dim-note">
            100% on-device. The index reads ONLY the folders you allowlist (it
            ships off and never scans your whole disk), and your file contents +
            embeddings NEVER leave this machine — the embedder is the on-device
            model and search runs locally. Results cite real indexed files; when
            the on-device embedder is down, search falls back to keyword (BM25)
            ranking and says so. Text-like files, born-digital PDFs and Office
            docs are extracted on-device; scanned/encrypted/corrupt files are
            skipped honestly, never guessed at. Say{" "}
            <b>&ldquo;index my documents&rdquo;</b> to (re)build the index,{" "}
            <b>&ldquo;search my files for&hellip;&rdquo;</b> to query it, and{" "}
            <b>&ldquo;forget my file index&rdquo;</b> to clear it.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The index-status row: file/chunk counts + the NEURAL vs BM25 verdict. When no
 *  index has been built yet, an honest "not indexed yet" line with the enable +
 *  allowlist + index steps (never a fake count). */
function IndexStatusRow({
  index,
  pdfJail,
}: {
  index: DocIndexStatus | null;
  pdfJail: boolean | null;
}) {
  if (index === null || index.chunks === 0) {
    return (
      <div className="docsearch-index">
        <div className="docsearch-index-head">
          <span className="docsearch-index-title">INDEX</span>
          <span className="docsearch-pill off">NOT INDEXED</span>
        </div>
        <div className="docsearch-empty dim-note">
          No files are indexed yet. On-device file search ships OFF — enable{" "}
          <code>[docsearch].enabled</code> and add a folder under{" "}
          <code>[docsearch].roots</code> in darwin.toml (it indexes only the
          folders you allowlist, never your whole disk), then say{" "}
          <b>&ldquo;index my documents&rdquo;</b>.
        </div>
      </div>
    );
  }

  // A real index exists. embedded_chunks === chunks (and chunks > 0) means every
  // chunk carries an on-device vector, so search runs NEURAL; fewer embedded
  // chunks means search over those falls back to BM25 (the embedder was down at
  // index time). Report whichever is true — never claim neural when it is not.
  const fullyEmbedded = index.embeddedChunks === index.chunks;
  return (
    <div className="docsearch-index">
      <div className="docsearch-index-head">
        <span className="docsearch-index-title">INDEX</span>
        <span
          className={`docsearch-pill ${fullyEmbedded ? "neural" : "bm25"}`}
          title={
            fullyEmbedded
              ? "every chunk has an on-device embedding — search runs neural (cosine over on-device vectors)"
              : "some chunks have no on-device embedding (the embedder was down at index time) — search over them falls back to lexical BM25; reindex with the embedder up to make it fully neural"
          }
        >
          {fullyEmbedded ? "NEURAL" : "BM25 FALLBACK"}
        </span>
        {/* The PDF-extraction guard that is ACTUALLY active (docsearch.status).
            Null = no status frame yet (an older daemon) — claim nothing. Amber
            when the daemon is on the weaker in-process guard, so a production
            install missing its pdfjail helper is never silently degraded. */}
        {pdfJail !== null && (
          <span
            className={`docsearch-pill ${pdfJail ? "jailed" : "unjailed"}`}
            title={
              pdfJail
                ? "PDF text extraction runs in the memory-jailed pdfjail subprocess — a decompression bomb aborts the short-lived helper child, never darwind"
                : "the pdfjail helper binary is missing next to darwind, so PDF extraction is on the weaker in-process guard (known filter-chain / parse-time bomb residuals) — rebuild the daemon (cargo build --release) or reinstall to restore the jail"
            }
          >
            {pdfJail ? "PDF JAIL ARMED" : "PDF JAIL MISSING"}
          </span>
        )}
      </div>
      <div className="docsearch-counts">
        <Count label="FILES" value={index.files} />
        <Count label="CHUNKS" value={index.chunks} />
        <Count label="EMBEDDED" value={index.embeddedChunks} total={index.chunks} />
      </div>
      {!fullyEmbedded && (
        <div className="docsearch-note dim-note">
          {index.embeddedChunks} of {index.chunks} chunks are embedded on-device;
          the rest will be ranked by keyword (BM25). Reindex while the on-device
          model is up to make search fully neural.
        </div>
      )}
    </div>
  );
}

/** One labelled count chip. `total` (when given) shows N / total so the
 *  embedded-vs-chunks ratio reads at a glance. */
function Count({
  label,
  value,
  total,
}: {
  label: string;
  value: number;
  total?: number;
}) {
  return (
    <div className="docsearch-count">
      <span className="docsearch-count-val">
        {value}
        {total !== undefined && <span className="docsearch-count-total"> / {total}</span>}
      </span>
      <span className="docsearch-count-label">{label}</span>
    </div>
  );
}

/** The last cited search result: the query, the method that actually ran, and
 *  the cited hits (real file + offset + snippet + score). An empty result is the
 *  honest "nothing found" — shown, never hidden or faked. */
function SearchResults({ search }: { search: DocSearchResult | null }) {
  if (search === null) return null;

  const neural = search.method === "neural-embedding";
  // Render the method honestly. Tolerant of an unknown future method string
  // (shown verbatim) so the panel never breaks on a new backend.
  const methodLabel =
    search.method === "neural-embedding"
      ? "NEURAL (on-device embeddings)"
      : search.method === "lexical-bm25"
        ? "LEXICAL (BM25 keyword)"
        : search.method.toUpperCase();

  return (
    <div className="docsearch-results">
      <div className="docsearch-results-head">
        <span className="docsearch-results-title">LAST SEARCH</span>
        <span
          className={`docsearch-pill ${neural ? "neural" : "bm25"}`}
          title={
            neural
              ? "ranked by cosine over on-device embedding vectors"
              : "ranked by BM25 keyword relevance (the on-device embedder was unavailable)"
          }
        >
          {methodLabel}
        </span>
      </div>

      {search.query.length > 0 && (
        <div className="docsearch-query">
          <span className="docsearch-query-label">QUERY</span>
          <span className="docsearch-query-text">{search.query}</span>
        </div>
      )}

      {search.hits.length === 0 ? (
        <div className="docsearch-empty dim-note">
          Nothing matched in your indexed files. This is the honest result — no
          file is invented. If you have not indexed yet, enable file search and
          allowlist a folder, then say <b>&ldquo;index my documents&rdquo;</b>.
        </div>
      ) : (
        <div className="docsearch-hits">
          {search.hits.map((h, i) => (
            <HitRow key={`${h.filePath}:${h.byteOffset}:${i}`} hit={h} />
          ))}
        </div>
      )}
    </div>
  );
}

/** One cited hit: the real file path + byte offset (the citation anchor), the
 *  relevance score, and the bounded snippet the daemon already cited. */
function HitRow({ hit }: { hit: DocHit }) {
  return (
    <div className="docsearch-hit">
      <div className="docsearch-hit-head">
        <span className="docsearch-hit-path" title={hit.filePath}>
          {hit.filePath}
        </span>
        <span className="docsearch-hit-offset" title="byte offset of the cited chunk in the file">
          @{hit.byteOffset}
        </span>
        <span className="docsearch-hit-score" title="relevance score (higher = more relevant)">
          {hit.score.toFixed(3)}
        </span>
      </div>
      <div className="docsearch-hit-snippet">{hit.snippet}</div>
    </div>
  );
}
