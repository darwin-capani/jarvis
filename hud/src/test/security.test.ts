import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import SettingsModal from "../components/SettingsModal";
import StatusBar from "../components/StatusBar";
import {
  parseSecurityStatus,
  securityLabel,
  securityTone,
  modelTierInitial,
  sttTierInitial,
  voiceIdInitial,
  voiceTierInitial,
  voiceModeInitial,
  type SecurityStatus,
  type TelemetryEnvelope,
} from "../core/events";
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
    ts: `2026-06-16T12:00:${String(counter % 60).padStart(2, "0")}Z`,
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

const noop = () => {};

/** The EXACT secret-free shape the daemon emits (daemon/src/main.rs
 *  `system / security.status`) when encryption is ACTIVE — the key actually
 *  resolved this run. Plus a hostile key-shaped field the daemon would never send,
 *  to pin the secret-free contract. */
const mockActive: Record<string, unknown> = {
  encrypt_memory_config: true,
  active: true,
  encrypted_stores: [
    "memory (facts/transcripts/episodes/events + world-model facts)",
    "docsearch index (chunk text + vectors)",
    "audit log",
    "optimizer trace store",
    "voiceid owner profile (encrypted blob wrapper)",
  ],
  not_encrypted: [
    "the config TOML",
    "the macOS Keychain item itself (already OS-protected)",
    "the in-RAM working set + decrypted pages + the key while darwind runs",
  ],
  honesty:
    "SQLCipher protects AT REST ON DISK only — not against a live-process/root attacker (key + plaintext are in RAM while running). The master key lives only in the macOS Keychain (account memory_encryption_key); lose it and the encrypted DBs are unrecoverable. Enabling changes the on-disk format (a one-time migration).",
  key_location: "macOS Keychain (account memory_encryption_key)",
  cipher: "SQLCipher AES-256 (transparent, whole-file, page-level)",
  // hostile extras a malformed/compromised payload might carry — the key MUST
  // never cross the wire, and the parser must never surface one.
  master_key: "0011223344556677889900112233445566778899001122334455667788990011",
  key: "sk-SECRET-MASTER-KEY",
};

/* ======================================================================== *
 * parseSecurityStatus (defensive, SECRET-FREE, fail-OFF)                     *
 * ======================================================================== */
describe("parseSecurityStatus (defensive, secret-free, fail-OFF)", () => {
  it("parses a well-formed ACTIVE security.status", () => {
    const s = parseSecurityStatus(mockActive);
    expect(s.config).toBe(true);
    expect(s.active).toBe(true);
    expect(s.encryptedStores.length).toBe(5);
    expect(s.encryptedStores[0]).toContain("memory");
    expect(s.notEncrypted.length).toBe(3);
    expect(s.notEncrypted.some((l) => l.includes("in-RAM"))).toBe(true);
    expect(s.cipher).toContain("SQLCipher AES-256");
    expect(s.keyLocation).toContain("memory_encryption_key");
    expect(s.honesty).toContain("AT REST ON DISK only");
  });

  it("NEVER surfaces the master key, even from a hostile payload", () => {
    const s = parseSecurityStatus(mockActive);
    const blob = JSON.stringify(s);
    expect(blob).not.toContain("master_key");
    expect(blob).not.toContain("sk-SECRET-MASTER-KEY");
    expect(blob).not.toContain("0011223344556677");
    // but the SECRET-FREE posture/scope/copy survive
    expect(blob).toContain("audit log");
    expect(blob).toContain("SQLCipher AES-256");
  });

  it("fails toward NOT-ACTIVE when `active` is absent/garbled (never a false green)", () => {
    expect(parseSecurityStatus({}).active).toBe(false);
    expect(parseSecurityStatus({ active: "yes" }).active).toBe(false);
    expect(parseSecurityStatus({ active: 1 }).active).toBe(false);
    // config defaults OFF too (the shipped posture)
    expect(parseSecurityStatus({}).config).toBe(false);
  });

  it("carries the HONEST config-on-but-key-failed posture (config true, active false)", () => {
    const s = parseSecurityStatus({ encrypt_memory_config: true, active: false });
    expect(s.config).toBe(true);
    expect(s.active).toBe(false);
    // the indicator is driven by active, so this reads NOT ENCRYPTED
    expect(securityLabel(s)).toBe("NOT ENCRYPTED");
    expect(securityTone(s)).toBe("idle");
  });

  it("defaults the scope arrays + copy safely and never throws on junk", () => {
    const s = parseSecurityStatus({
      encrypted_stores: "nope",
      not_encrypted: [1, "the config TOML", null],
      honesty: 42,
    });
    expect(s.encryptedStores).toEqual([]);
    // non-string entries dropped, the real string kept
    expect(s.notEncrypted).toEqual(["the config TOML"]);
    expect(s.honesty).toBe("");
    expect(() => parseSecurityStatus({ active: {} })).not.toThrow();
  });
});

/* ======================================================================== *
 * securityTone / securityLabel (ground-truth driven)                         *
 * ======================================================================== */
describe("securityTone / securityLabel (ground-truth, never config alone)", () => {
  it("ENCRYPTED AT REST + on tone only when the key actually resolved", () => {
    const s = parseSecurityStatus(mockActive);
    expect(securityLabel(s)).toBe("ENCRYPTED AT REST");
    expect(securityTone(s)).toBe("on");
  });

  it("NOT ENCRYPTED + idle tone for the shipped-OFF default", () => {
    const s = parseSecurityStatus({ encrypt_memory_config: false, active: false });
    expect(securityLabel(s)).toBe("NOT ENCRYPTED");
    expect(securityTone(s)).toBe("idle");
  });

  it("NOT ENCRYPTED even when config is ON but the key failed (no false green)", () => {
    const s = parseSecurityStatus({ encrypt_memory_config: true, active: false });
    expect(securityLabel(s)).toBe("NOT ENCRYPTED");
    expect(securityTone(s)).toBe("idle");
  });
});

/* ======================================================================== *
 * Reducer arm                                                               *
 * ======================================================================== */
describe("security.status reducer", () => {
  it("is null before any snapshot (honest awaiting)", () => {
    expect(connected().security).toBeNull();
  });

  it("sets the security posture from a well-formed event (secret-free)", () => {
    const s = tel(connected(), env("security.status", mockActive));
    expect(s.security).not.toBeNull();
    expect(s.security!.active).toBe(true);
    expect(s.security!.encryptedStores.length).toBe(5);
    // the master key never reaches state
    expect(JSON.stringify(s.security)).not.toContain("sk-SECRET-MASTER-KEY");
    expect(JSON.stringify(s.security)).not.toContain("master_key");
  });

  it("a malformed payload yields the honest fail-OFF snapshot, not a stale one", () => {
    const s = tel(connected(), env("security.status", { active: "garbage" }));
    expect(s.security).not.toBeNull();
    expect(s.security!.active).toBe(false);
    expect(s.security!.config).toBe(false);
  });

  it("the OFF (shipped-default) snapshot is the honest NOT-ENCRYPTED posture", () => {
    const s = tel(
      connected(),
      env("security.status", { encrypt_memory_config: false, active: false }),
    );
    expect(s.security!.active).toBe(false);
    expect(securityLabel(s.security!)).toBe("NOT ENCRYPTED");
  });
});

/* ======================================================================== *
 * StatusBar encryption chip (honest, ground-truth driven)                    *
 * ======================================================================== */
function renderStatusBar(security: SecurityStatus | null): string {
  return renderToStaticMarkup(
    createElement(StatusBar, {
      connected: true,
      coreState: "idle" as const,
      cloudKeyPresent: true,
      inferenceOffline: false,
      heal: null,
      cloudModel: null,
      activeAgent: null,
      voiceId: voiceIdInitial(),
      modelTier: modelTierInitial(),
      voiceTier: voiceTierInitial(),
      sttTier: sttTierInitial(),
      voiceMode: voiceModeInitial(),
      security,
      onOpenSettings: noop,
      onOpenDeck: noop,
    }),
  );
}

describe("StatusBar encryption chip", () => {
  it("renders nothing before the snapshot arrives (not cluttered)", () => {
    const html = renderStatusBar(null);
    expect(html).not.toContain("encryption-chip");
    expect(html).not.toContain("ENCRYPTED");
  });

  it("renders ENCRYPTED in the on tone when the key actually resolved", () => {
    const html = renderStatusBar(parseSecurityStatus(mockActive));
    expect(html).toContain("encryption-chip");
    expect(html).toContain("on");
    expect(html).toContain("ENCRYPTED");
    // the honest hover copy: at-rest-on-disk only, the cipher + key location
    expect(html).toContain("AT REST ON DISK only");
    expect(html).toContain("SQLCipher AES-256");
    expect(html).toContain("memory_encryption_key");
  });

  it("renders NOT ENCRYPTED in the idle tone for the shipped-OFF default", () => {
    const html = renderStatusBar(
      parseSecurityStatus({ encrypt_memory_config: false, active: false }),
    );
    expect(html).toContain("NOT ENCRYPTED");
    expect(html).toContain("idle");
  });

  it("renders NOT ENCRYPTED + an honest key-failed note when config-on but key failed", () => {
    const html = renderStatusBar(
      parseSecurityStatus({ encrypt_memory_config: true, active: false }),
    );
    expect(html).toContain("NOT ENCRYPTED");
    expect(html.toLowerCase()).toContain("did not resolve");
  });

  it("never renders the master key, even from a hostile snapshot", () => {
    const html = renderStatusBar(parseSecurityStatus(mockActive));
    expect(html).not.toContain("sk-SECRET-MASTER-KEY");
    expect(html).not.toContain("master_key");
    expect(html).not.toContain("0011223344556677");
  });
});

/* ======================================================================== *
 * SettingsModal — ENCRYPTION section (honest, scope-exact, never overclaims)  *
 * ======================================================================== */
function renderSettings(security: SecurityStatus | null): string {
  return renderToStaticMarkup(
    createElement(SettingsModal, {
      mcp: null,
      voiceId: voiceIdInitial(),
      modelTier: modelTierInitial(),
      sttTier: sttTierInitial(),
      security,
      onClose: noop,
    }),
  );
}

describe("SettingsModal encryption section (honest, scope-exact)", () => {
  it("shows the section with the indicator + the [security] enable key", () => {
    const html = renderSettings(parseSecurityStatus(mockActive));
    expect(html).toContain("ENCRYPTION");
    expect(html).toContain("AT-REST");
    expect(html).toContain("ENCRYPTED AT REST");
    // the exact config key + the enable steps
    expect(html).toContain("encrypt_memory");
    expect(html).toContain("[security]");
    expect(html.toLowerCase()).toContain("restart darwind");
  });

  it("renders the AWAITING state before the snapshot arrives", () => {
    const html = renderSettings(null);
    expect(html).toContain("AWAITING");
    // the section + the documentation still render (it reflects the config key)
    expect(html).toContain("encrypt_memory");
  });

  it("renders NOT ENCRYPTED for the shipped-OFF default", () => {
    const html = renderSettings(
      parseSecurityStatus({ encrypt_memory_config: false, active: false }),
    );
    expect(html).toContain("NOT ENCRYPTED");
  });

  it("states EXACTLY which stores are encrypted (does NOT overclaim)", () => {
    const html = renderSettings(parseSecurityStatus(mockActive));
    // the four sensitive SQLite stores + the voiceid blob
    expect(html.toLowerCase()).toContain("docsearch");
    expect(html.toLowerCase()).toContain("audit log");
    expect(html.toLowerCase()).toContain("optimizer trace");
    expect(html.toLowerCase()).toContain("voiceid owner profile");
    // never the dishonest blanket claim
    expect(html.toLowerCase()).not.toContain("all your data is encrypted");
  });

  it("states EXACTLY what is NOT encrypted (config TOML, Keychain item, in-RAM)", () => {
    const html = renderSettings(parseSecurityStatus(mockActive));
    expect(html.toLowerCase()).toContain("config toml");
    expect(html.toLowerCase()).toContain("keychain item");
    expect(html.toLowerCase()).toContain("in-ram working set");
  });

  it("surfaces the HONEST limits: at-rest-only, lose-the-key, ships-OFF, migration", () => {
    const html = renderSettings(parseSecurityStatus(mockActive));
    // protects at rest on disk ONLY — not a live-process/root attacker
    expect(html.toLowerCase()).toContain("at rest on disk only");
    expect(html.toLowerCase()).toContain("live-process");
    // the key is Keychain-held; lose it => lose the data
    expect(html.toLowerCase()).toContain("unrecoverable");
    expect(html).toContain("memory_encryption_key");
    // ships OFF + enabling changes the on-disk format (migration)
    expect(html.toLowerCase()).toContain("ships off");
    expect(html.toLowerCase()).toContain("migration");
  });

  it("the section is config-documentation only — it does not pretend the HUD writes the key/config", () => {
    const html = renderSettings(parseSecurityStatus(mockActive));
    expect(html.toLowerCase()).toContain("the hud never writes daemon config");
    expect(html.toLowerCase()).toContain("never holds the key");
  });

  it("renders an honest key-failed note when config-on but the key failed", () => {
    const html = renderSettings(
      parseSecurityStatus({
        encrypt_memory_config: true,
        active: false,
        encrypted_stores: [],
        not_encrypted: [],
      }),
    );
    expect(html).toContain("NOT ENCRYPTED");
    expect(html.toLowerCase()).toContain("plaintext");
    expect(html.toLowerCase()).toContain("check the keychain");
  });

  it("never renders the master key, even from a hostile snapshot", () => {
    const html = renderSettings(parseSecurityStatus(mockActive));
    expect(html).not.toContain("sk-SECRET-MASTER-KEY");
    expect(html).not.toContain("master_key");
    expect(html).not.toContain("0011223344556677");
  });
});
