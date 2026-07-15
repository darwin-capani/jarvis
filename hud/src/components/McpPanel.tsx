import type { McpServerStatus, McpStatus } from "../core/events";
import Frame from "./Frame";

/**
 * MCP // EXTERNAL TOOLS — the review/visibility surface for the Model Context
 * Protocol external-tool servers (daemon/src/mcp.rs). It lists every CONFIGURED
 * server, its transport, its connection status, the tools it exposes (badging a
 * CONSEQUENTIAL tool — one that parks behind the confirmation gate — distinctly
 * from a read-only one), and the per-server agent allowlist (which DARWIN agents
 * may use that server's tools).
 *
 * SAFETY CONTRACT (do not regress):
 *   - REVIEW-ONLY. There is NO button here that connects a server, runs a tool,
 *     or changes a setting. MCP is the most dangerous external surface in DARWIN
 *     (a server runs code on the user's machine); this panel only SHOWS the
 *     surface so the user can see what is wired and gated.
 *   - NEVER renders a token/secret. The wire snapshot carries only `usesToken`
 *     as a bool (the daemon strips the value/account before emitting), and this
 *     component renders that bool as a "TOKEN" pill — never any secret material.
 *   - SHIPPED-OFF honest. With `[mcp].enabled = false` (the default) the panel
 *     shows an explicit "MCP is off" state, not a blank or a fake.
 *
 * The reducer only ever sets `mcp` from a defensively-parsed `mcp.status` event
 * (parseMcpStatus drops malformed entries and never returns a secret), so this
 * component can trust the fields it is handed.
 */
export default function McpPanel({ mcp }: { mcp: McpStatus | null }) {
  // No snapshot yet (daemon has not emitted mcp.status) — render nothing rather
  // than a placeholder, mirroring the other event-fed panels.
  if (mcp === null) return null;

  return (
    <div className="mcp-panel">
      <Frame title="MCP // EXTERNAL TOOLS" tag="REVIEW ONLY">
        <div className="mcp-body">
          {!mcp.enabled ? (
            <div className="mcp-off dim-note">
              MCP is OFF. No external tool server is connected and no MCP tool is
              offered to any agent. Enable <code>[mcp].enabled</code> in
              darwin.toml and configure a server to turn it on.
            </div>
          ) : mcp.servers.length === 0 ? (
            <div className="mcp-off dim-note">
              MCP is on, but no server is configured. Add a server under{" "}
              <code>[[mcp.servers]]</code> in darwin.toml — until then nothing
              connects and no MCP tool exists.
            </div>
          ) : (
            mcp.servers.map((s) => <ServerRow key={s.name} server={s} />)
          )}

          <div className="mcp-foot dim-note">
            External tool servers run on your machine. They are bounded and
            per-agent allowlisted, and stdio servers run under a default-deny
            sandbox profile that constrains — but does not fully neutralize — an
            untrusted server. A CONSEQUENTIAL tool parks for a spoken
            confirmation before it can act; a read-only tool runs ungated.
            Tokens live in the Keychain and are never shown here.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** One configured server: name + transport + connection status, its tools, and
 *  the agents allowlisted to use it. Renders ONLY secret-free fields. */
function ServerRow({ server }: { server: McpServerStatus }) {
  const connected = server.connected;
  return (
    <div className="mcp-server">
      <div className="mcp-server-head">
        <span className="mcp-server-name">{server.name}</span>
        <span className="mcp-server-transport">{server.transport.toUpperCase()}</span>
        {server.usesToken ? (
          <span className="mcp-pill token" title="this server uses a Keychain token (value never shown)">
            TOKEN
          </span>
        ) : null}
        <span className={`mcp-pill ${connected ? "ok" : "off"}`}>
          {connected ? "CONNECTED" : "NOT CONNECTED"}
        </span>
      </div>

      {/* Tools the server exposes — empty until/unless connected. */}
      <div className="mcp-tools">
        {server.tools.length === 0 ? (
          <span className="mcp-empty dim-note">
            {connected ? "no tools exposed" : "tools appear once connected"}
          </span>
        ) : (
          server.tools.map((t) => (
            <span
              key={t.name}
              className={`mcp-tool ${t.consequential ? "conseq" : "ro"}`}
              title={
                t.consequential
                  ? "consequential — parks for a spoken confirmation before it acts"
                  : "read-only — runs ungated"
              }
            >
              {t.name}
              <i className="mcp-tool-class" aria-hidden="true">
                {t.consequential ? "GATED" : "RO"}
              </i>
            </span>
          ))
        )}
      </div>

      {/* The per-server agent allowlist — who may use this server's tools. The
          orchestrator (darwin) is always implicitly admitted; the daemon's
          config lists the additional agents shown here. */}
      <div className="mcp-agents">
        <span className="mcp-agents-label">AGENTS</span>
        {server.agents.length === 0 ? (
          <span className="mcp-empty dim-note">orchestrator only</span>
        ) : (
          server.agents.map((a) => (
            <span key={a} className="mcp-agent">
              {a}
            </span>
          ))
        )}
      </div>
    </div>
  );
}
