import { describe, expect, it } from "vitest";
import { ErrorBoundary } from "../components/ErrorBoundary";

// The full render/fallback path needs a DOM (vitest runs in the `node`
// environment here), so this covers the correctness-critical CAPTURE logic: a
// panel throwing (e.g. `.toFixed()` on an undefined value from an edge telemetry
// frame) must be turned into boundary state so a LOCAL fallback renders — instead
// of React 18 unmounting the entire HUD tree to an unrecoverable blank screen.
describe("ErrorBoundary", () => {
  it("getDerivedStateFromError surfaces the caught error into fallback state", () => {
    const err = new Error("panel boom");
    expect(ErrorBoundary.getDerivedStateFromError(err)).toEqual({ error: err });
  });
});
