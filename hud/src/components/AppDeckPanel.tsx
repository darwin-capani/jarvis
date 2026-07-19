import type { AppFeed } from "../core/state";
import type { AppRegistryEntry } from "../core/events";
import Frame from "./Frame";

/**
 * APP // DECK — the live micro-app console: the sandboxed Python capability
 * modules in apps/ rendered as a premium glass deck, GROUPED BY CATEGORY, each
 * card showing what the app does, its exposed tool, and whether it is LIVE
 * (running + connected) or IDLE, cross-referenced against the daemon's
 * app.started / app.stopped / app.data relays (state.runningApps / appFeeds).
 *
 * SAFETY CONTRACT (do not regress):
 *   - REVIEW-ONLY. Nothing here launches, stops, or drives an app — launching a
 *     sandboxed process is the daemon's gated job; this only SHOWS the fleet + its
 *     live status.
 *   - SECRET-FREE. Static catalog metadata + the manifest NAME + the running flag.
 *
 * An app not yet discovered simply reads IDLE (honest — never a fabricated "live").
 */

type DeckApp = {
  /** manifest name = apps/<id>/ dir; the key into runningApps / appFeeds. */
  id: string;
  name: string;
  desc: string;
  /** the manifest-declared read-only tool this app exposes. */
  tool: string;
  /** category bucket (display grouping). */
  cat: string;
};

// Categories in display order (AI leads — the on-device-LLM apps). OTHER is the
// bucket for any app the daemon discovers that this curated catalog doesn't know
// yet — so a NEW app auto-appears rather than being invisible until hand-listed.
const CATEGORIES = ["AI", "DEV", "DATA", "SECURITY", "NETWORK", "ENGINEERING", "TEXT", "TIME", "DESIGN", "OTHER"] as const;

// The curated micro-app fleet (each a real, validated capability module in apps/).
const FLEET: DeckApp[] = [
  // On-device AI (local LLM via the daemon generate proxy — offline, no cloud).
  { id: "summarize", name: "Summarize", desc: "Condense text with the local LLM", tool: "summarize.run", cat: "AI" },
  { id: "classify", name: "Classify", desc: "Label text into your categories", tool: "classify.run", cat: "AI" },
  { id: "extract", name: "Extract", desc: "Pull fields as JSON from text", tool: "extract.run", cat: "AI" },
  { id: "rewrite", name: "Rewrite", desc: "Rewrite text in any tone", tool: "rewrite.run", cat: "AI" },
  { id: "explain", name: "Explain", desc: "Explain a code snippet in plain English", tool: "explain.run", cat: "AI" },
  { id: "keywords", name: "Keywords", desc: "Extract key terms & tags", tool: "keywords.run", cat: "AI" },
  { id: "titlegen", name: "Titlegen", desc: "Generate a headline for text", tool: "titlegen.run", cat: "AI" },
  { id: "sentiment", name: "Sentiment", desc: "Sentiment + a one-line reason", tool: "sentiment.run", cat: "AI" },
  { id: "codeglass", name: "Codeglass", desc: "Code metrics — line, comment & TODO density", tool: "codeglass.metrics", cat: "DEV" },
  { id: "jsonpath", name: "JSONPath", desc: "Query a JSON document by path", tool: "jsonpath.query", cat: "DEV" },
  { id: "regexpad", name: "RegexPad", desc: "Test a regex, see matches & groups", tool: "regexpad.test", cat: "DEV" },
  { id: "diffscope", name: "Diffscope", desc: "Unified line diff of two texts", tool: "diffscope.unified", cat: "DEV" },
  { id: "datalint", name: "Datalint", desc: "JSON validate & structure inspect", tool: "datalint.inspect", cat: "DATA" },
  { id: "csvlens", name: "CSVLens", desc: "Profile a CSV — rows, cols, nulls", tool: "csvlens.profile", cat: "DATA" },
  { id: "numbase", name: "Numbase", desc: "Number base / radix converter", tool: "numbase.convert", cat: "DATA" },
  { id: "hashkit", name: "Hashkit", desc: "MD5 / SHA-1 / SHA-256 digests", tool: "hashkit.digest", cat: "SECURITY" },
  { id: "jwtpeek", name: "JWTPeek", desc: "Decode a JWT (no signature verify)", tool: "jwtpeek.decode", cat: "SECURITY" },
  { id: "entropy", name: "Entropy", desc: "Secret strength — bits & class", tool: "entropy.assess", cat: "SECURITY" },
  { id: "textkit", name: "Textkit", desc: "Text statistics & readability", tool: "textkit.stats", cat: "TEXT" },
  { id: "markmap", name: "Markmap", desc: "Markdown heading outline / TOC", tool: "markmap.outline", cat: "TEXT" },
  { id: "cronwise", name: "Cronwise", desc: "Cron expression, explained", tool: "cronwise.explain", cat: "TIME" },
  { id: "timewarp", name: "Timewarp", desc: "Unix epoch → UTC calendar time", tool: "timewarp.convert", cat: "TIME" },
  { id: "colorlab", name: "Colorlab", desc: "Color science + WCAG contrast", tool: "colorlab.analyze", cat: "DESIGN" },
  // Network toolkit (pure/offline analyzers — no app opens a socket; net_hosts=[] in every manifest).
  { id: "subnetcalc", name: "Subnetcalc", desc: "IPv4/IPv6 CIDR planner + VLSM split", tool: "subnet.plan", cat: "NETWORK" },
  { id: "cidrtool", name: "CIDRTool", desc: "Aggregate CIDRs + overlap analysis", tool: "cidr.aggregate", cat: "NETWORK" },
  { id: "urlparse", name: "URLParse", desc: "Dissect a URL — parts, IDN, warnings", tool: "url.dissect", cat: "NETWORK" },
  { id: "portref", name: "Portref", desc: "Well-known port & service reference", tool: "port.lookup", cat: "NETWORK" },
  // Engineering bench (pure calculators — exact formulas, honest errors).
  { id: "ohmslaw", name: "OhmsLaw", desc: "V·I·R·P solver with SI-unit parsing", tool: "ohm.solve", cat: "ENGINEERING" },
  { id: "resistor", name: "Resistor", desc: "Color-band decoder + E24/E96 series", tool: "resistor.decode", cat: "ENGINEERING" },
  { id: "unitwise", name: "Unitwise", desc: "Unit converter across 10 categories", tool: "unit.convert", cat: "ENGINEERING" },
  { id: "freqwave", name: "Freqwave", desc: "Wavelength·LC·RC resonance solver", tool: "wave.solve", cat: "ENGINEERING" },
];

/** The curated catalog indexed by app id — the source of the nice display name +
 *  category + copy for KNOWN apps. An app the daemon discovers that isn't here
 *  still renders (in OTHER) from its live manifest metadata. */
const CATALOG_BY_ID = new Map(FLEET.map((a) => [a.id, a]));

/** Title-case an unknown app id for its display name ("share-guard" -> "Share Guard"). */
function titleCase(id: string): string {
  return id
    .split(/[-_]/)
    .filter(Boolean)
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
    .join(" ");
}

/** Trim a manifest description to a card-sized blurb (the manifest one can be a
 *  long honest sentence). Cuts on a word boundary near the cap. */
function blurb(desc: string): string {
  const d = desc.trim();
  if (d.length <= 64) return d;
  const cut = d.slice(0, 64);
  const sp = cut.lastIndexOf(" ");
  return `${(sp > 40 ? cut.slice(0, sp) : cut).trimEnd()}…`;
}

/** The apps to RENDER: the LIVE registry when the daemon has reported it (a new
 *  app auto-appears), else the curated FLEET (fallback for an old daemon / before
 *  the first frame). A live app keeps its curated card when known, else shows its
 *  real manifest metadata in OTHER. */
function renderApps(registry: AppRegistryEntry[]): DeckApp[] {
  if (registry.length === 0) return FLEET;
  return registry.map((r) => {
    const known = CATALOG_BY_ID.get(r.id);
    if (known) return known;
    return {
      id: r.id,
      name: titleCase(r.id),
      desc: r.description.length > 0 ? blurb(r.description) : "on-device micro-app",
      tool: r.tool,
      cat: "OTHER",
    };
  });
}

export default function AppDeckPanel({
  runningApps,
  appFeeds,
  manifestIssues = [],
  appRegistry = [],
}: {
  runningApps: ReadonlySet<string>;
  appFeeds: Record<string, AppFeed>;
  /** app.manifest_invalid lines ("dir: error") — apps the daemon SKIPPED at
   *  discovery because their manifest.toml failed to parse/validate. Rendered
   *  as install errors so a broken manifest is visible, not a silent absence. */
  manifestIssues?: string[];
  /** The live app catalog (app.registry). When present the deck renders from it
   *  (new apps auto-appear); empty -> the curated FLEET fallback. */
  appRegistry?: AppRegistryEntry[];
}) {
  const FLEET_LIVE = renderApps(appRegistry);
  const isLive = (id: string) => runningApps.has(id) || appFeeds[id]?.running === true;
  const liveCount = FLEET_LIVE.filter((a) => isLive(a.id)).length;

  return (
    <div className="deck-panel">
      <Frame title="APP // DECK" tag="REVIEW ONLY">
        <div className="deck-body">
          <div className="deck-summary">
            <span className="deck-count">
              <span className="deck-live-n">{liveCount}</span>
              <span className="deck-slash"> / </span>
              <span className="deck-total-n">{FLEET_LIVE.length}</span>
            </span>
            <span className="deck-count-label">LIVE</span>
          </div>

          {CATEGORIES.map((cat) => {
            const apps = FLEET_LIVE.filter((a) => a.cat === cat);
            if (apps.length === 0) return null;
            const catLive = apps.filter((a) => isLive(a.id)).length;
            return (
              <div className="deck-group" key={cat}>
                <div className="deck-group-head">
                  <span className="deck-group-label">{cat}</span>
                  <span className="deck-group-rule" aria-hidden="true" />
                  <span className="deck-group-count">
                    {catLive}/{apps.length}
                  </span>
                </div>
                <div className="deck-grid">
                  {apps.map((a, i) => {
                    const live = isLive(a.id);
                    return (
                      <div
                        key={a.id}
                        className={`deck-card ${live ? "live" : "idle"}`}
                        style={{ animationDelay: `${i * 40}ms` }}
                      >
                        <div className="deck-card-head">
                          <span className={`deck-dot ${live ? "live" : "idle"}`} aria-hidden="true" />
                          <span className="deck-name">{a.name}</span>
                        </div>
                        <div className="deck-desc">{a.desc}</div>
                        <div className="deck-foot">
                          <span className="deck-tool">{a.tool}</span>
                          <span className={`deck-state ${live ? "live" : "idle"}`}>
                            {live ? "LIVE" : "IDLE"}
                          </span>
                        </div>
                      </div>
                    );
                  })}
                </div>
              </div>
            );
          })}

          {manifestIssues.length > 0 && (
            <div className="deck-issues">
              <div className="deck-issues-title">MANIFEST ERRORS</div>
              {manifestIssues.map((m) => (
                <div className="deck-issue" key={m} title={m}>
                  {m}
                </div>
              ))}
            </div>
          )}

          <div className="deck-note dim-note">
            Sandboxed, offline, read-only capability modules (apps/). Review-only —
            launching a module is the daemon's gated action, not this panel's.
          </div>
        </div>
      </Frame>
    </div>
  );
}
