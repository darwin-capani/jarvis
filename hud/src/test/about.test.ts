import { describe, it, expect } from "vitest";
import { aboutCheckView } from "../core/about";
import type { UpdateCheck } from "../tauri/bridge";

/* The About panel's "Check for Updates" result mapping. Mirrors the backend
 * UpdateCheck contract (updates.rs) and the honesty rules in core/about.ts. */

const mk = (status: UpdateCheck["status"], detail = "", version?: string | null): UpdateCheck => ({
  status,
  detail,
  version,
});

describe("aboutCheckView", () => {
  it("up_to_date -> the reassuring latest-version line (the user's explicit ask)", () => {
    const v = aboutCheckView(mk("up_to_date", "DARWIN is on the latest version."));
    expect(v.kind).toBe("uptodate");
    expect(v.text).toBe("DARWIN is on the latest version.");
    expect(v.version).toBeNull();
  });

  it("available WITH a version -> routes (kind available + the real version)", () => {
    const v = aboutCheckView(mk("available", "Version 1.1.0 is available.", "1.1.0"));
    expect(v.kind).toBe("available");
    expect(v.version).toBe("1.1.0");
    expect(v.text).toContain("1.1.0");
  });

  it("available WITHOUT a version -> never routes (no fabricated install target)", () => {
    const v = aboutCheckView(mk("available", "An update is available.", null));
    expect(v.kind).toBe("error");
    expect(v.version).toBeNull();
  });

  it("error -> surfaces the backend's honest detail, error styling", () => {
    const v = aboutCheckView(mk("error", "endpoint unreachable"));
    expect(v.kind).toBe("error");
    expect(v.text).toBe("endpoint unreachable");
    expect(v.version).toBeNull();
  });

  it("not_configured -> honest detail, never claims latest", () => {
    const v = aboutCheckView(mk("not_configured", "Updates aren't configured for this build."));
    expect(v.kind).toBe("error");
    expect(v.text).toBe("Updates aren't configured for this build.");
  });

  it("unavailable (browser/no shell) -> honest detail", () => {
    const v = aboutCheckView(mk("unavailable", "Updates are checked from the DARWIN desktop app."));
    expect(v.kind).toBe("error");
    expect(v.version).toBeNull();
  });

  it("installed -> stays honest (up to date), no fabricated version", () => {
    const v = aboutCheckView(mk("installed", "Installed and verified.", "1.1.0"));
    expect(v.kind).toBe("uptodate");
    expect(v.version).toBeNull();
  });

  it("falls back to a safe line when detail is empty", () => {
    expect(aboutCheckView(mk("error", "")).text).toBe("Could not check for updates.");
  });
});
