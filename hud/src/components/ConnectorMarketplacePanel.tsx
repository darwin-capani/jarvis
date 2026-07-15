import type { McpStatus } from "../core/events";
import Frame from "./Frame";

/**
 * CONNECTOR // MARKETPLACE — the browsable front door of the self-extension
 * engine: a curated catalog of vetted Model-Context-Protocol (MCP) connectors
 * DARWIN can plug into, each cross-referenced against the LIVE configured set so
 * you can see what is already CONFIGURED vs AVAILABLE (and the credential each
 * needs).
 *
 * SAFETY CONTRACT (do not regress):
 *   - REVIEW-ONLY. There is NO button here that adds a connector, writes config,
 *     stores a token, or runs a server. Adding an MCP connector is a CONSEQUENTIAL
 *     mutation of the most dangerous external surface in DARWIN (an MCP server
 *     runs code); it must go through the gated path (scoped config write +
 *     Keychain token + a read-only tools/list handshake before arming). This
 *     panel only SHOWS what is available so the user can decide.
 *   - SECRET-FREE. It renders only static catalog metadata + the NAME of the
 *     credential a connector would need — never any secret value. The configured
 *     cross-reference uses only server names from the defensively-parsed
 *     mcp.status snapshot (which already strips tokens).
 */

type Connector = {
  /** Distinctive match key against configured MCP server names. */
  id: string;
  name: string;
  desc: string;
  /** The credential this connector needs to arm, or null for none. Never a value. */
  token: string | null;
};

// A curated catalog of real, well-known MCP connectors (the official
// modelcontextprotocol servers + widely-used community ones). Static reference
// data — recommendations, not live state.
const CATALOG: Connector[] = [
  { id: "filesystem", name: "Filesystem", desc: "Read/write files within allowlisted roots", token: null },
  { id: "github", name: "GitHub", desc: "Repos, issues, PRs, code search", token: "GitHub PAT" },
  { id: "postgres", name: "Postgres", desc: "Read-only SQL over a Postgres database", token: "connection string" },
  { id: "sqlite", name: "SQLite", desc: "Query a local SQLite database", token: null },
  { id: "slack", name: "Slack", desc: "Channels, messages, search", token: "bot token" },
  { id: "google-maps", name: "Google Maps", desc: "Places, directions, geocoding", token: "Maps API key" },
  { id: "brave-search", name: "Brave Search", desc: "Web + local search results", token: "Brave API key" },
  { id: "fetch", name: "Fetch", desc: "Fetch a URL and convert it to clean text", token: null },
  { id: "puppeteer", name: "Puppeteer", desc: "Headless-browser navigation + scraping", token: null },
  { id: "memory", name: "Memory", desc: "Persistent knowledge-graph memory", token: null },
  { id: "sentry", name: "Sentry", desc: "Inspect issues + error events", token: "auth token" },
  { id: "linear", name: "Linear", desc: "Issues, projects, cycles", token: "API key" },
  { id: "notion", name: "Notion", desc: "Pages, databases, search", token: "integration token" },
  { id: "sequential-thinking", name: "Sequential Thinking", desc: "Structured step-by-step reasoning", token: null },
];

/** True when a configured MCP server name corresponds to this connector — an
 *  exact or word-boundary match (so the `github` connector is never falsely
 *  marked configured by a server merely named with that substring elsewhere, and
 *  the dropped-standalone `git` collision is avoided entirely). */
function isConfigured(serverNames: string[], id: string): boolean {
  return serverNames.some(
    (n) =>
      n === id ||
      n.startsWith(`${id}-`) ||
      n.startsWith(`${id}_`) ||
      n.endsWith(`-${id}`) ||
      n.endsWith(`_${id}`),
  );
}

export default function ConnectorMarketplacePanel({ mcp }: { mcp: McpStatus | null }) {
  // The catalog is static reference data — always meaningful. mcp only ENRICHES
  // it with which connectors are already configured (empty set until the snapshot
  // arrives, so everything reads AVAILABLE — honest, never stale).
  const serverNames = (mcp?.servers ?? []).map((s) => s.name.toLowerCase());
  const rows = CATALOG.map((c) => ({ ...c, configured: isConfigured(serverNames, c.id) }));
  const connected = rows.filter((c) => c.configured).length;

  return (
    <div className="connectors-panel">
      <Frame title="CONNECTOR // MARKETPLACE" tag="REVIEW ONLY">
        <div className="connectors-body">
          <div className="connectors-summary">
            <span className="connectors-count">
              <span className="connectors-on-n">{connected}</span>
              <span className="connectors-slash"> / </span>
              <span className="connectors-total-n">{CATALOG.length}</span>
            </span>
            <span className="connectors-count-label">CONFIGURED</span>
          </div>
          <div className="connectors-note dim-note">
            Vetted MCP connectors DARWIN can plug into. Adding one is a gated action
            (scoped config entry + Keychain token + a read-only handshake before it
            arms) — review-only here.
          </div>
          {rows.map((c) => (
            <div key={c.id} className={`connector-row ${c.configured ? "on" : "off"}`}>
              <span className="connector-dot" aria-hidden="true" />
              <span className="connector-name">{c.name}</span>
              <span className="connector-desc" title={c.desc}>
                {c.desc}
              </span>
              <span className="connector-token">{c.token ? `needs ${c.token}` : "no key"}</span>
              <span className={`connector-pill ${c.configured ? "on" : "off"}`}>
                {c.configured ? "CONFIGURED" : "AVAILABLE"}
              </span>
            </div>
          ))}
        </div>
      </Frame>
    </div>
  );
}
