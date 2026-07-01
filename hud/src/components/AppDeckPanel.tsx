import type { AppFeed } from "../core/state";
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

// Categories in display order.
const CATEGORIES = ["DEV", "DATA", "SECURITY", "TEXT", "TIME", "DESIGN"] as const;

// The curated micro-app fleet (each a real, validated capability module in apps/).
const FLEET: DeckApp[] = [
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
];

export default function AppDeckPanel({
  runningApps,
  appFeeds,
}: {
  runningApps: ReadonlySet<string>;
  appFeeds: Record<string, AppFeed>;
}) {
  const isLive = (id: string) => runningApps.has(id) || appFeeds[id]?.running === true;
  const liveCount = FLEET.filter((a) => isLive(a.id)).length;

  return (
    <div className="deck-panel">
      <Frame title="APP // DECK" tag="REVIEW ONLY">
        <div className="deck-body">
          <div className="deck-summary">
            <span className="deck-count">
              <span className="deck-live-n">{liveCount}</span>
              <span className="deck-slash"> / </span>
              <span className="deck-total-n">{FLEET.length}</span>
            </span>
            <span className="deck-count-label">LIVE</span>
          </div>

          {CATEGORIES.map((cat) => {
            const apps = FLEET.filter((a) => a.cat === cat);
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

          <div className="deck-note dim-note">
            Sandboxed, offline, read-only capability modules (apps/). Review-only —
            launching a module is the daemon's gated action, not this panel's.
          </div>
        </div>
      </Frame>
    </div>
  );
}
