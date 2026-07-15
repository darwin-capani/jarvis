import { describe, expect, it } from "vitest";
import {
  DEFAULT_AGENT_HUE,
  PRIME_AGENT,
  ROSTER,
  agentProfile,
  normalizeHue,
} from "../core/agents";

/* The static roster mirror (CONTRACT part C.1) — must stay lockstep with the
 * canonical daemon map (config/agents.toml). These tests pin the invariants
 * the panel + reducer rely on. */

describe("agent roster (static mirror)", () => {
  it("has exactly the 27 canonical agents", () => {
    expect(ROSTER).toHaveLength(27);
  });

  it("names are unique, lowercase, and include the full roster", () => {
    const names = ROSTER.map((a) => a.name);
    expect(new Set(names).size).toBe(names.length);
    for (const n of names) expect(n).toBe(n.toLowerCase());
    expect(names).toEqual([
      "darwin",
      "friday",
      "veronica",
      "vision",
      "ultron",
      "athena",
      "stark",
      "steve",
      "oracle",
      "gecko",
      "hercules",
      "pepper",
      "hulk",
      "herald",
      "jerome",
      "edith",
      "fury",
      "cassandra",
      "mnemosyne",
      "sage",
      "vitalis",
      "karen",
      "dume",
      "midas",
      "voyager",
      "aegis",
      "babel",
    ]);
  });

  it("darwin is first (roll-call order) and is the PRIME_AGENT with the default hue", () => {
    expect(ROSTER[0].name).toBe(PRIME_AGENT);
    expect(ROSTER[0].hue).toBe(DEFAULT_AGENT_HUE);
  });

  it("every hue is an integer in [0, 360)", () => {
    for (const a of ROSTER) {
      expect(Number.isInteger(a.hue)).toBe(true);
      expect(a.hue).toBeGreaterThanOrEqual(0);
      expect(a.hue).toBeLessThan(360);
    }
  });

  it("every agent carries a non-empty role and voice", () => {
    for (const a of ROSTER) {
      expect(a.role.length).toBeGreaterThan(0);
      expect(a.voice.length).toBeGreaterThan(0);
    }
  });

  it("ultron uses deep-orange 15, NOT the reserved alert-red 0", () => {
    // RED (hue 0) is reserved for alerts on this HUD; ultron's identity hue is
    // the deep-orange 15 the contract specifies.
    const ultron = agentProfile("ultron");
    expect(ultron?.hue).toBe(15);
    // and no agent may claim the reserved red.
    for (const a of ROSTER) expect(a.hue).not.toBe(0);
  });
});

describe("agentProfile lookup", () => {
  it("resolves a known agent", () => {
    expect(agentProfile("vision")?.role).toBe("Research + OSINT");
    expect(agentProfile("vision")?.hue).toBe(265);
  });

  it("is case-insensitive and trims whitespace", () => {
    expect(agentProfile("  VISION ")?.name).toBe("vision");
    expect(agentProfile("Darwin")?.name).toBe("darwin");
  });

  it("returns null for an unknown agent", () => {
    expect(agentProfile("loki")).toBeNull();
    expect(agentProfile("")).toBeNull();
  });
});

describe("normalizeHue", () => {
  it("rounds and wraps into [0, 360)", () => {
    expect(normalizeHue(0)).toBe(0);
    expect(normalizeHue(359.6)).toBe(0); // rounds to 360 -> wraps to 0
    expect(normalizeHue(190.4)).toBe(190);
    expect(normalizeHue(375)).toBe(15);
    expect(normalizeHue(-40)).toBe(320);
    expect(normalizeHue(720 + 90)).toBe(90);
  });

  it("falls back for non-finite input", () => {
    expect(normalizeHue(Number.NaN)).toBe(DEFAULT_AGENT_HUE);
    expect(normalizeHue(Infinity)).toBe(DEFAULT_AGENT_HUE);
    expect(normalizeHue(Number.NaN, 50)).toBe(50);
  });
});
