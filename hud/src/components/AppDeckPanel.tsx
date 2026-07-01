import type { AppFeed } from "../core/state";
import Frame from "./Frame";

/**
 * APP // DECK — the live micro-app console: the sandboxed Python capability
 * modules in apps/ rendered as a premium glass deck, each card showing what the
 * app does, its exposed tool, and whether it is LIVE (running + connected) or
 * IDLE, cross-referenced against the daemon's app.started/app.stopped +
 * app.data relays (state.runningApps / state.appFeeds).
 *
 * SAFETY CONTRACT (do not regress):
 *   - REVIEW-ONLY. Nothing here launches, stops, or drives an app — launching a
 *     sandboxed process is the daemon's gated job; this only SHOWS the fleet and
 *     its live status so the user can see what capability modules exist and which
 *     are running.
 *   - SECRET-FREE. The catalog is static metadata; the live cross-reference uses
 *     only the manifest NAME + the running flag + the last-activity timestamp.
 *
 * The catalog is the curated fleet; an app not yet discovered simply reads IDLE
 * (honest — never a fabricated "live").
 */

type DeckApp = {
  /** manifest name = apps/<id>/ dir; the key into runningApps / appFeeds. */
  id: string;
  name: string;
  desc: string;
  /** the manifest-declared read-only tool this app exposes. */
  tool: string;
  /** short category tag for the card accent. */
  kind: string;
};

// The curated micro-app fleet (each a real, validated capability module in apps/).
const FLEET: DeckApp[] = [
  { id: "codeglass", name: "Codeglass", desc: "Code metrics — line, comment & TODO density", tool: "codeglass.metrics", kind: "CODE" },
  { id: "textkit", name: "Textkit", desc: "Text statistics & readability", tool: "textkit.stats", kind: "TEXT" },
  { id: "hashkit", name: "Hashkit", desc: "MD5 / SHA-1 / SHA-256 digests", tool: "hashkit.digest", kind: "CRYPTO" },
  { id: "datalint", name: "Datalint", desc: "JSON validate & structure inspect", tool: "datalint.inspect", kind: "DATA" },
  { id: "colorlab", name: "Colorlab", desc: "Color science + WCAG contrast", tool: "colorlab.analyze", kind: "COLOR" },
  { id: "cronwise", name: "Cronwise", desc: "Cron expression, explained", tool: "cronwise.explain", kind: "TIME" },
  { id: "numbase", name: "Numbase", desc: "Number base / radix converter", tool: "numbase.convert", kind: "MATH" },
];

export default function AppDeckPanel({
  runningApps,
  appFeeds,
}: {
  runningApps: ReadonlySet<string>;
  appFeeds: Record<string, AppFeed>;
}) {
  const rows = FLEET.map((a) => ({
    ...a,
    live: runningApps.has(a.id) || appFeeds[a.id]?.running === true,
  }));
  const liveCount = rows.filter((r) => r.live).length;

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
          <div className="deck-grid">
            {rows.map((a, i) => (
              <div
                key={a.id}
                className={`deck-card ${a.live ? "live" : "idle"}`}
                style={{ animationDelay: `${i * 40}ms` }}
              >
                <div className="deck-card-head">
                  <span className={`deck-dot ${a.live ? "live" : "idle"}`} aria-hidden="true" />
                  <span className="deck-name">{a.name}</span>
                  <span className="deck-kind">{a.kind}</span>
                </div>
                <div className="deck-desc">{a.desc}</div>
                <div className="deck-foot">
                  <span className="deck-tool">{a.tool}</span>
                  <span className={`deck-state ${a.live ? "live" : "idle"}`}>
                    {a.live ? "LIVE" : "IDLE"}
                  </span>
                </div>
              </div>
            ))}
          </div>
          <div className="deck-note dim-note">
            Sandboxed, offline, read-only capability modules (apps/). Review-only —
            launching a module is the daemon's gated action, not this panel's.
          </div>
        </div>
      </Frame>
    </div>
  );
}
