import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import McpPanel from "../components/McpPanel";
import {
  parseMcpStatus,
  type McpStatus,
  type TelemetryEnvelope,
} from "../core/events";
import { mcpTokenAccount } from "../core/credentials";
import { HudState, initialState, reduce } from "../core/state";

/* helpers ------------------------------------------------------------------ */

let counter = 0;
function env(
  event: string,
  data: Record<string, unknown> = {},
  source = "system",
): TelemetryEnvelope {
  counter += 1;
  return {
    ts: `2026-06-15T12:00:${String(counter % 60).padStart(2, "0")}Z`,
    source,
    event,
    data,
  };
}

function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

function connected(at = 0): HudState {
  return reduce(initialState(), { type: "ws.connected", at });
}

/** A realistic mock mcp.status payload: one connected token-using server with a
 *  read-only and a consequential tool + an agent allowlist, and one configured-
 *  but-unconnected server. */
const mockPayload: Record<string, unknown> = {
  enabled: true,
  servers: [
    {
      name: "files",
      transport: "stdio",
      connected: true,
      uses_token: true,
      agents: ["friday", "pepper"],
      tools: [
        { name: "read_file", consequential: false },
        { name: "write_file", consequential: true },
      ],
    },
    {
      name: "weather",
      transport: "http",
      connected: false,
      uses_token: false,
      agents: [],
      tools: [],
    },
  ],
};

/* ------------------------------------------------------------------------ *
 * The defensive parser. NEVER returns null (an MCP frame always yields an    *
 * honest snapshot), NEVER carries a secret, drops malformed entries.         *
 * ------------------------------------------------------------------------ */
describe("parseMcpStatus (defensive)", () => {
  it("parses a well-formed mcp.status payload", () => {
    const s = parseMcpStatus(mockPayload);
    expect(s.enabled).toBe(true);
    expect(s.servers.length).toBe(2);
    const files = s.servers.find((x) => x.name === "files")!;
    expect(files.transport).toBe("stdio");
    expect(files.connected).toBe(true);
    expect(files.usesToken).toBe(true);
    expect(files.agents).toEqual(["friday", "pepper"]);
    expect(files.tools).toEqual([
      { name: "read_file", consequential: false },
      { name: "write_file", consequential: true },
    ]);
    const weather = s.servers.find((x) => x.name === "weather")!;
    expect(weather.connected).toBe(false);
    expect(weather.tools).toEqual([]);
  });

  it("defaults to the shipped-OFF snapshot when fields are absent", () => {
    const s = parseMcpStatus({});
    expect(s.enabled).toBe(false);
    expect(s.servers).toEqual([]);
  });

  it("drops malformed server + tool entries, never throws", () => {
    const s = parseMcpStatus({
      enabled: "yes", // non-bool -> false
      servers: [
        { transport: "stdio" }, // no name -> dropped
        42, // non-object -> dropped
        {
          name: "ok",
          tools: [{ consequential: true }, { name: "t", consequential: false }, "junk"],
          agents: ["a", 7, "b"], // non-string dropped
        },
      ],
    });
    expect(s.enabled).toBe(false);
    expect(s.servers.length).toBe(1);
    const ok = s.servers[0];
    expect(ok.name).toBe("ok");
    // The nameless tool + the "junk" string are dropped; only the named one survives.
    expect(ok.tools).toEqual([{ name: "t", consequential: false }]);
    expect(ok.agents).toEqual(["a", "b"]);
    // A tool with no consequential bool defaults to consequential (fail-safe) —
    // but it has no name here, so it was dropped; assert the default on a named one.
    const s2 = parseMcpStatus({ servers: [{ name: "x", tools: [{ name: "u" }] }] });
    expect(s2.servers[0].tools[0]).toEqual({ name: "u", consequential: true });
  });

  it("NEVER surfaces a token/secret field", () => {
    const s = parseMcpStatus({
      enabled: true,
      servers: [
        {
          name: "files",
          uses_token: true,
          // Hostile extra fields a malformed/compromised payload might carry —
          // the parser surfaces ONLY the known secret-free fields.
          token: "sk-SECRET",
          bearer: "leak",
          url: "https://user:pw@host",
          tools: [{ name: "t", consequential: false, secret: "nope" }],
        },
      ],
    });
    const blob = JSON.stringify(s);
    expect(blob).not.toContain("SECRET");
    expect(blob).not.toContain("leak");
    expect(blob).not.toContain("token"); // no token key survives
    expect(blob).not.toContain("user:pw");
    // usesToken is a bool, present and true.
    expect(s.servers[0].usesToken).toBe(true);
  });

  it("never throws on junk", () => {
    expect(() => parseMcpStatus({ servers: "nope" })).not.toThrow();
    expect(parseMcpStatus({ servers: "nope" }).servers).toEqual([]);
  });
});

/* ------------------------------------------------------------------------ *
 * The reducer arm. An mcp.status event ALWAYS sets a snapshot (honest off /  *
 * on state); a malformed payload yields an empty snapshot, never a stale one.*
 * ------------------------------------------------------------------------ */
describe("mcp.status reducer", () => {
  it("sets the MCP snapshot from a well-formed event", () => {
    const s = tel(connected(), env("mcp.status", mockPayload));
    expect(s.mcp).not.toBeNull();
    expect(s.mcp!.enabled).toBe(true);
    expect(s.mcp!.servers.length).toBe(2);
  });

  it("sets an honest OFF snapshot when the subsystem is disabled", () => {
    const s = tel(connected(), env("mcp.status", { enabled: false, servers: [] }));
    expect(s.mcp).not.toBeNull();
    expect(s.mcp!.enabled).toBe(false);
    expect(s.mcp!.servers).toEqual([]);
  });

  it("never stores a secret in state", () => {
    const s = tel(
      connected(),
      env("mcp.status", {
        enabled: true,
        servers: [{ name: "files", uses_token: true, token: "sk-SECRET", bearer: "leak" }],
      }),
    );
    const blob = JSON.stringify(s.mcp);
    expect(blob).not.toContain("SECRET");
    expect(blob).not.toContain("leak");
    expect(s.mcp!.servers[0].usesToken).toBe(true);
  });
});

/* ------------------------------------------------------------------------ *
 * The panel (rendered headlessly, node env). REVIEW-ONLY + secret-free.      *
 * ------------------------------------------------------------------------ */
describe("McpPanel (review-only, secret-free)", () => {
  const render = (mcp: McpStatus | null) =>
    renderToStaticMarkup(createElement(McpPanel, { mcp }));

  it("renders nothing before any snapshot", () => {
    expect(render(null)).toBe("");
  });

  it("shows the honest OFF state when disabled", () => {
    const html = render(parseMcpStatus({ enabled: false, servers: [] }));
    expect(html).toMatch(/MCP is OFF/i);
    expect(html).toContain("REVIEW ONLY");
  });

  it("renders servers, transport, connection status, tools, and agents", () => {
    const html = render(parseMcpStatus(mockPayload));
    expect(html).toContain("files");
    expect(html).toContain("STDIO");
    expect(html).toContain("CONNECTED");
    expect(html).toContain("NOT CONNECTED"); // the unconnected weather server
    expect(html).toContain("read_file");
    expect(html).toContain("write_file");
    // The consequential tool is badged GATED; the read-only one RO.
    expect(html).toContain("GATED");
    expect(html).toContain("RO");
    // The per-server agent allowlist.
    expect(html).toContain("friday");
    expect(html).toContain("pepper");
    // A token-using server shows a TOKEN pill — but NEVER the token value.
    expect(html).toContain("TOKEN");
  });

  it("has NO action button — it is review-only", () => {
    const html = render(parseMcpStatus(mockPayload));
    expect(html).not.toContain("<button");
  });

  it("never renders a token/secret value", () => {
    const html = render(
      parseMcpStatus({
        enabled: true,
        servers: [
          {
            name: "files",
            uses_token: true,
            token: "sk-SECRET",
            bearer: "leak",
            tools: [{ name: "t", consequential: false }],
          },
        ],
      }),
    );
    expect(html).not.toContain("SECRET");
    expect(html).not.toContain("leak");
  });

  it("caps a hostile oversized mcp.status payload (servers / agents / tools)", () => {
    const s = parseMcpStatus({
      enabled: true,
      servers: Array.from({ length: 500 }, (_, i) => ({
        name: `srv${i}`,
        agents: Array.from({ length: 500 }, (_, a) => `agent${a}`),
        tools: Array.from({ length: 2000 }, (_, t) => ({ name: `tool${t}`, consequential: false })),
      })),
    });
    expect(s.servers.length).toBe(64); // MCP_SERVERS_CAP
    expect(s.servers[0].agents.length).toBe(64); // MCP_AGENTS_CAP
    expect(s.servers[0].tools.length).toBe(256); // MCP_TOOLS_CAP
  });
});

/* ------------------------------------------------------------------------ *
 * The settings token-account helper — bounded to the strict server shape so  *
 * a hostile name can never produce a Keychain account to write to.           *
 * ------------------------------------------------------------------------ */
describe("mcpTokenAccount", () => {
  it("mints mcp_<server>_token for a valid server name", () => {
    expect(mcpTokenAccount("files")).toBe("mcp_files_token");
    expect(mcpTokenAccount("weather-api")).toBe("mcp_weather-api_token");
    expect(mcpTokenAccount("a_b_2")).toBe("mcp_a_b_2_token");
  });

  it("returns null for a hostile / malformed server name", () => {
    expect(mcpTokenAccount("")).toBeNull();
    expect(mcpTokenAccount("../../etc")).toBeNull();
    expect(mcpTokenAccount("a b")).toBeNull();
    expect(mcpTokenAccount("Files")).toBeNull(); // uppercase rejected
    expect(mcpTokenAccount("a\0b")).toBeNull();
  });

  it("rejects `__` and any leading/trailing/consecutive separator (namespacing)", () => {
    // `weather__api` would mis-split the flat id mcp__weather__api__<tool>.
    expect(mcpTokenAccount("weather__api")).toBeNull();
    expect(mcpTokenAccount("a__b")).toBeNull();
    expect(mcpTokenAccount("a--b")).toBeNull();
    expect(mcpTokenAccount("a_-b")).toBeNull();
    expect(mcpTokenAccount("_files")).toBeNull();
    expect(mcpTokenAccount("files_")).toBeNull();
    expect(mcpTokenAccount("-files")).toBeNull();
    expect(mcpTokenAccount("files-")).toBeNull();
  });
});
