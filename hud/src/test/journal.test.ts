import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import JournalPanel from "../components/JournalPanel";
import { parseJournalSnapshot, type TelemetryEnvelope } from "../core/events";
import { initialState, reduce } from "../core/state";

let counter = 0;
function env(event: string, data: Record<string, unknown>, source = "system"): TelemetryEnvelope {
  counter += 1;
  return { ts: `2026-07-12T00:00:${String(counter % 60).padStart(2, "0")}Z`, source, event, data };
}
function connected() {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}

const entry = {
  ts: "2026-07-12T00:00:01Z",
  agent: "agent.ads",
  tool: "gads_pause_campaign",
  preview: "Would pause campaign 42.",
  undoable: true,
  note: "",
  undone: false,
  via: "confirm",
};

describe("parseJournalSnapshot (never over-claims reversibility)", () => {
  it("parses a well-formed snapshot", () => {
    const j = parseJournalSnapshot({ count: 3, entries: [entry] });
    expect(j.count).toBe(3);
    expect(j.entries).toEqual([entry]);
  });

  it("degrades a garbled frame to an honest empty ledger", () => {
    expect(parseJournalSnapshot({})).toEqual({ count: 0, entries: [] });
    expect(parseJournalSnapshot({ entries: "nope", count: "many" })).toEqual({
      count: 0,
      entries: [],
    });
  });

  it("drops malformed rows and never invents undoable", () => {
    const j = parseJournalSnapshot({
      entries: [
        { ...entry, undoable: "yes", undone: 1 }, // non-boolean flags coerce to false
        { preview: "no tool row" }, // no tool -> dropped
        42, // not an object -> dropped
      ],
    });
    expect(j.entries).toHaveLength(1);
    expect(j.entries[0].undoable).toBe(false);
    expect(j.entries[0].undone).toBe(false);
    // count falls back to what actually parsed when absent.
    expect(j.count).toBe(1);
  });
});

describe("journal.snapshot reducer", () => {
  it("is null until the first frame, then populated", () => {
    const s0 = connected();
    expect(s0.journal).toBeNull();
    const s1 = reduce(s0, {
      type: "telemetry",
      envelope: env("journal.snapshot", { count: 1, entries: [entry] }),
      at: 1000,
    });
    expect(s1.journal).not.toBeNull();
    expect(s1.journal?.count).toBe(1);
    expect(s1.journal?.entries[0].tool).toBe("gads_pause_campaign");
  });
});

describe("JournalPanel", () => {
  const render = (journal: Parameters<typeof JournalPanel>[0]["journal"]) =>
    renderToStaticMarkup(createElement(JournalPanel, { journal }));

  it("renders nothing before the first frame", () => {
    expect(render(null)).toBe("");
  });

  it("says so honestly when nothing executed this session", () => {
    const html = render({ count: 0, entries: [] });
    expect(html).toContain("no consequential actions executed this session");
  });

  it("shows honest pills: UNDOABLE, NO UNDO with reason, UNDONE", () => {
    const html = render({
      count: 3,
      entries: [
        entry,
        {
          ...entry,
          tool: "gmail_send",
          preview: "Would send an email to x@y.z.",
          undoable: false,
          note: "sent mail can't be unsent",
        },
        { ...entry, tool: "meta_pause_campaign", undone: true },
      ],
    });
    expect(html).toContain("UNDOABLE");
    expect(html).toContain("NO UNDO");
    expect(html).toContain("UNDONE");
    expect(html).toContain("sent mail can&#x27;t be unsent");
    expect(html).toContain("gads_pause_campaign");
    expect(html).toContain("3 executed this session");
    expect(html).toContain("undo that");
  });

  it("marks policy-approved rows distinctly", () => {
    const html = render({ count: 1, entries: [{ ...entry, via: "policy" }] });
    expect(html).toContain("auto-approved");
  });
});
