import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ConnectorMarketplacePanel from "../components/ConnectorMarketplacePanel";
import type { McpStatus } from "../core/events";

const render = (mcp: McpStatus | null) =>
  renderToStaticMarkup(createElement(ConnectorMarketplacePanel, { mcp }));

describe("ConnectorMarketplacePanel (review-only)", () => {
  it("renders the curated catalog even before any mcp snapshot", () => {
    const html = render(null);
    expect(html).toContain("CONNECTOR // MARKETPLACE");
    expect(html).toContain("REVIEW ONLY");
    expect(html).toContain("GitHub");
    expect(html).toContain("Postgres");
    // Nothing configured yet -> all AVAILABLE, no connector pill marked configured.
    // (The word "CONFIGURED" still appears as the summary count LABEL — so we
    // assert on the pill class, not the bare word.)
    expect(html).toContain("AVAILABLE");
    expect(html).not.toContain("connector-pill on");
  });

  it("marks a connector CONFIGURED when a matching server is configured", () => {
    const mcp: McpStatus = {
      enabled: true,
      servers: [
        { name: "github", transport: "stdio", connected: true, usesToken: true, agents: [], tools: [] },
        { name: "my-postgres", transport: "stdio", connected: false, usesToken: true, agents: [], tools: [] },
      ],
    };
    const html = render(mcp);
    expect(html).toContain("CONFIGURED");
    // GitHub (exact) and Postgres (word-boundary "my-postgres") both resolve.
    // Two configured of the catalog total.
    expect(html).toContain(">2<"); // the configured count
  });

  it("does NOT false-positive the substring collision (github !== git-anything)", () => {
    // A server named 'github' must not mark a hypothetical 'git' connector; we
    // dropped standalone 'git', but verify 'github' only configures GitHub by
    // checking exactly one CONFIGURED pill results.
    const mcp: McpStatus = {
      enabled: true,
      servers: [{ name: "github", transport: "stdio", connected: true, usesToken: true, agents: [], tools: [] }],
    };
    const html = render(mcp);
    // Exactly one CONFIGURED connector pill (GitHub) — no substring false-positives.
    // (Count the pill class, not the word, which also appears in the summary label.)
    expect(html.match(/connector-pill on/g)?.length ?? 0).toBe(1);
  });

  it("shows the credential each connector needs, never a secret value", () => {
    const html = render(null);
    expect(html).toContain("needs GitHub PAT");
    expect(html).toContain("no key"); // a no-credential connector
  });

  it("is review-only — renders no action button", () => {
    expect(render(null)).not.toContain("<button");
  });
});
