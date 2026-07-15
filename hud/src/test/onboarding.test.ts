import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import OnboardingWizard from "../components/OnboardingWizard";
import {
  ONBOARDING_SEEN_KEY,
  ONBOARDING_SEEN_VALUE,
  ONBOARDING_STEPS,
  ONBOARDING_STEP_COUNT,
  clampStep,
  hasSeenOnboarding,
  isLastStep,
  markOnboardingSeen,
  type OnboardingStorage,
} from "../core/onboarding";

/* helpers ------------------------------------------------------------------ */

/** An in-memory localStorage stub so the once-only flag can be tested in the
 *  node vitest environment (no real DOM/localStorage). */
function memStorage(): OnboardingStorage & { map: Map<string, string> } {
  const map = new Map<string, string>();
  return {
    map,
    getItem: (k) => (map.has(k) ? map.get(k)! : null),
    setItem: (k, v) => {
      map.set(k, v);
    },
  };
}

const noop = () => {};

/* ======================================================================== *
 * The once-only flag: shows once, then respects the persisted flag.          *
 * ======================================================================== */
describe("onboarding once-only flag", () => {
  it("a fresh install has NOT seen onboarding -> the wizard should show", () => {
    const store = memStorage();
    expect(hasSeenOnboarding(store)).toBe(false);
  });

  it("after markOnboardingSeen it IS seen -> the wizard never shows again", () => {
    const store = memStorage();
    expect(hasSeenOnboarding(store)).toBe(false);
    markOnboardingSeen(store);
    expect(hasSeenOnboarding(store)).toBe(true);
    // The persisted value is the fixed literal under the versioned key.
    expect(store.getItem(ONBOARDING_SEEN_KEY)).toBe(ONBOARDING_SEEN_VALUE);
  });

  it("markOnboardingSeen is idempotent (calling twice stays seen)", () => {
    const store = memStorage();
    markOnboardingSeen(store);
    markOnboardingSeen(store);
    expect(hasSeenOnboarding(store)).toBe(true);
  });

  it("FAIL-SAFE: with no storage it reads as already-seen (never re-nags)", () => {
    expect(hasSeenOnboarding(null)).toBe(true);
    // and a write against null storage is a harmless no-op (never throws)
    expect(() => markOnboardingSeen(null)).not.toThrow();
  });

  it("FAIL-SAFE: a storage that throws on read reads as already-seen", () => {
    const throwing: OnboardingStorage = {
      getItem: () => {
        throw new Error("blocked");
      },
      setItem: noop,
    };
    expect(hasSeenOnboarding(throwing)).toBe(true);
  });

  it("a different key/value does NOT count as seen (only the exact flag dismisses)", () => {
    const store = memStorage();
    store.setItem(ONBOARDING_SEEN_KEY, "0");
    expect(hasSeenOnboarding(store)).toBe(false);
    store.setItem("some.other.key", ONBOARDING_SEEN_VALUE);
    expect(hasSeenOnboarding(store)).toBe(false);
  });
});

/* ======================================================================== *
 * Step model: bounds + routing targets are EXISTING surfaces only.           *
 * ======================================================================== */
describe("onboarding step model", () => {
  it("clampStep keeps Back/Next inside the deck", () => {
    expect(clampStep(-5)).toBe(0);
    expect(clampStep(0)).toBe(0);
    expect(clampStep(ONBOARDING_STEP_COUNT + 99)).toBe(ONBOARDING_STEP_COUNT - 1);
    expect(clampStep(NaN)).toBe(0);
    expect(clampStep(2.9)).toBe(2);
  });

  it("isLastStep is true only on the final step", () => {
    expect(isLastStep(0)).toBe(false);
    expect(isLastStep(ONBOARDING_STEP_COUNT - 1)).toBe(true);
    // clamps first, so an out-of-range index is the last step
    expect(isLastStep(ONBOARDING_STEP_COUNT + 5)).toBe(true);
  });

  it("EVERY routing step targets an EXISTING settings surface (never a new one)", () => {
    const allowed = new Set([null, "settings-system", "settings-credentials"]);
    for (const step of ONBOARDING_STEPS) {
      expect(allowed.has(step.route)).toBe(true);
      // a routing step always carries an action label; an info step never does
      if (step.route === null) {
        expect(step.actionLabel).toBeNull();
      } else {
        expect(step.actionLabel).not.toBeNull();
      }
    }
  });

  it("the first step is an intro and the last is a recap (both info-only)", () => {
    expect(ONBOARDING_STEPS[0].route).toBeNull();
    expect(ONBOARDING_STEPS[ONBOARDING_STEP_COUNT - 1].route).toBeNull();
  });

  it("the copy is HONEST about the gated posture (armed-but-gated, autonomy OFF)", () => {
    const blob = ONBOARDING_STEPS.flatMap((s) => [s.title, ...s.body]).join(" ").toLowerCase();
    expect(blob).toContain("gated");
    expect(blob).toContain("off"); // autonomy ships off
    expect(blob).toContain("keychain"); // the key is never logged/shown
    // never overclaims that DARWIN will act for the user without the gate
    expect(blob).toContain("never");
  });
});

/* ======================================================================== *
 * Render: real steps, Skip everywhere, routing wires to the callback.        *
 * ======================================================================== */
describe("OnboardingWizard render", () => {
  function html(onRoute = noop, onDismiss = noop) {
    return renderToStaticMarkup(
      createElement(OnboardingWizard, {
        onRoute: onRoute as never,
        onDismiss,
      }),
    );
  }

  it("renders the first step with the honest armed-but-gated intro", () => {
    const out = html();
    expect(out).toContain("FIRST-RUN SETUP");
    expect(out).toContain(ONBOARDING_STEPS[0].title);
    expect(out.toLowerCase()).toContain("gated");
    // the real step count, never a faked total
    expect(out).toContain(`of ${ONBOARDING_STEP_COUNT}`);
  });

  it("always shows the Skip / I'll do this later affordance", () => {
    const out = html();
    expect(out.toLowerCase()).toContain("skip");
    expect(out.toLowerCase()).toContain("later");
  });

  it("shows Next (not Finish) on the first step, with no Back", () => {
    const out = html();
    expect(out).toContain(">Next<");
    expect(out).not.toContain(">Finish<");
    expect(out).not.toContain(">Back<");
  });

  it("does not render the routing action on an info-only first step", () => {
    // The intro step has no route, so no 'Open ...' primary action appears yet.
    const out = html();
    expect(out).not.toContain("Open System Settings");
    expect(out).not.toContain("Open Credentials");
  });
});

/* ======================================================================== *
 * App wiring contract: dismiss persists, route deep-opens an existing tab.   *
 * (Exercised at the unit level via the same helpers App uses, since the      *
 * node vitest env does not mount App; this pins the once-only + routing      *
 * contract the App callbacks rely on.)                                       *
 * ======================================================================== */
describe("onboarding wiring contract (the App callbacks rely on these)", () => {
  it("dismiss-then-reload: once seen, the gate that App uses returns false", () => {
    const store = memStorage();
    // App computes the initial open state as !hasSeenOnboarding().
    expect(!hasSeenOnboarding(store)).toBe(true); // first run -> shows
    // Dismiss (Skip / Finish / Esc / backdrop / route-away) persists the flag.
    markOnboardingSeen(store);
    // A later launch reads the persisted flag and does NOT show again.
    expect(!hasSeenOnboarding(store)).toBe(false);
  });

  it("a routing step's onRoute is called with an EXISTING-surface target", () => {
    // The component maps step.route straight through; assert the only non-null
    // targets are the two existing tabs the App routes to SettingsModal.
    const routeTargets = ONBOARDING_STEPS.map((s) => s.route).filter((r) => r !== null);
    expect(routeTargets.length).toBeGreaterThan(0);
    for (const t of routeTargets) {
      expect(["settings-system", "settings-credentials"]).toContain(t);
    }
    const onRoute = vi.fn();
    // Smoke: rendering with a spy callback does not invoke it (no auto-route).
    renderToStaticMarkup(
      createElement(OnboardingWizard, { onRoute, onDismiss: noop }),
    );
    expect(onRoute).not.toHaveBeenCalled();
  });
});
