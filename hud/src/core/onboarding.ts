/* ======================================================================== *
 * FIRST-RUN ONBOARDING — the pure step model + the persisted "seen" flag.    *
 *                                                                            *
 * WS4b item (1). The wizard is shown ONCE on first run and then never again  *
 * (a way to RE-OPEN it from Settings is fine). It honestly explains the      *
 * shipped posture — ARMED BUT GATED — and then ROUTES the user to the        *
 * EXISTING, already-gated surfaces (System Settings for TCC guidance + the   *
 * file-search/code RAG roots editors, the Credentials tab for the cloud key  *
 * + OAuth + voice enrolment). It NEVER duplicates those surfaces, NEVER adds  *
 * a write/act path, and NEVER bypasses a confirmation / enrolment /          *
 * permission step — every "route" just OPENS one of the existing panels on   *
 * the relevant tab; the user still does the gated action there.             *
 *                                                                            *
 * This module is the PURE, DOM-free core (step content + the flag), so the   *
 * step copy + the once-only flag can be unit-tested in the node vitest       *
 * environment exactly like the reducer/parsers. The React component          *
 * (OnboardingWizard.tsx) renders these steps and calls the routing callback. *
 * ======================================================================== */

/** localStorage key for the once-only first-run flag. Versioned so a future
 *  re-onboarding (new surfaces worth re-explaining) can ship by bumping the
 *  suffix without colliding with an already-dismissed v1. */
export const ONBOARDING_SEEN_KEY = "darwin.onboarding.seen.v1";

/** The value written when the wizard has been completed/skipped. A presence
 *  check is enough; the literal is fixed so a test can assert it exactly. */
export const ONBOARDING_SEEN_VALUE = "1";

/** Where a step's primary action ROUTES. Each target maps to an EXISTING,
 *  already-gated surface — the wizard opens it, it never reimplements it.
 *   - "settings-system"      : the System Settings tab (config/darwin.toml editor):
 *                              TCC guidance lives here, plus the file-search +
 *                              code RAG-roots editors and voice enrolment controls.
 *   - "settings-credentials" : the Credentials & Gates tab: the cloud key, OAuth
 *                              connect rows, MCP tokens, the policy editor, and
 *                              the voice-id review.
 *   - null                   : an informational step with no routing (the intro /
 *                              the closing recap) — only Skip/Back/Next/Finish.
 */
export type OnboardingRouteTarget = "settings-system" | "settings-credentials" | null;

/** One onboarding step. `body` lines are rendered as separate paragraphs so the
 *  copy stays scannable. `route` (when non-null) drives the primary action that
 *  OPENS the named existing surface; `actionLabel` is that button's text. Every
 *  step ALSO carries the always-available "Skip / I'll do this later" — that is
 *  rendered by the component, not encoded per-step. */
export interface OnboardingStep {
  /** Stable id (for keys + tests). */
  id: string;
  /** Short step title. */
  title: string;
  /** Body paragraphs (honest, no overclaim). */
  body: string[];
  /** Which existing surface the primary action opens, or null (info-only step). */
  route: OnboardingRouteTarget;
  /** The primary action button label when `route` is non-null. */
  actionLabel: string | null;
}

/**
 * The onboarding steps, in order. HONESTY-FIRST: the copy describes the REAL
 * shipped posture (armed-but-gated, autonomy OFF by default, on-device first)
 * and never promises a capability is on. Every routing step points at an
 * existing surface and says plainly that the gated action happens THERE.
 */
export const ONBOARDING_STEPS: readonly OnboardingStep[] = [
  {
    id: "welcome",
    title: "Welcome — DARWIN is armed but gated",
    body: [
      "DARWIN can see, hear, and act — but every consequential or outward action is GATED. Nothing reaches the world without passing the confirmation gate, the master switch, and (when enabled) the owner voice-id check.",
      "Autonomy is ARMED but propose-only: self-heal, the app forge, and the optimizer are enabled by default, yet they only ever PROPOSE — each one drafts a change and waits for you to apply it, never acting on its own. Answers stay on-device unless you opt into a cloud tier.",
      "This quick tour just points you at the existing setup surfaces. It never changes a setting or grants a permission for you — you do each gated step yourself.",
    ],
    route: null,
    actionLabel: null,
  },
  {
    id: "permissions",
    title: "macOS permissions (TCC)",
    body: [
      "To hear you and see the screen, macOS must grant Microphone, Accessibility, and Screen Recording to the DARWIN app — in System Settings → Privacy & Security. macOS asks you directly; DARWIN cannot grant these for you.",
      "Open System Settings below for the in-app guidance on which permissions each capability needs. Until you grant them, those capabilities simply stay inert — nothing fails silently.",
    ],
    route: "settings-system",
    actionLabel: "Open System Settings",
  },
  {
    id: "cloud-key",
    title: "Cloud key & integrations (optional)",
    body: [
      "The on-device model answers with no cloud call. To use the cloud HEAVY/FAST tiers, add your Anthropic key on the Credentials tab — it is stored in the macOS Keychain only, never logged or shown back.",
      "Integrations (Google, X, LinkedIn, GitHub, Slack, MCP servers) connect on the same tab. Connecting an account only stores a credential; every action it later enables still passes the consequential gate.",
    ],
    route: "settings-credentials",
    actionLabel: "Open Credentials",
  },
  {
    id: "file-search",
    title: "File search & code roots (optional)",
    body: [
      "On-device file/code search is OFF and indexes NOTHING until you allowlist a folder. Contents and embeddings never leave the device.",
      "Add the folders DARWIN may index under System Settings → File-search folders and Code-intelligence roots. Each entry is an absolute path you choose explicitly — there is no whole-disk scan.",
    ],
    route: "settings-system",
    actionLabel: "Open System Settings",
  },
  {
    id: "voice-id",
    title: "Voice enrolment (optional)",
    body: [
      "Voice-id is an on-device speaker check that RAISES the bar on outward actions. It is a lightweight acoustic match, not a biometric, and it is an added layer on top of the confirmation gate — never a replacement.",
      "Enrolment is an explicit step you trigger yourself: open Credentials and use the Voice-id controls, or say \"enroll my voice\" and repeat the prompted phrases. Raw audio is never stored or uploaded.",
    ],
    route: "settings-credentials",
    actionLabel: "Open Credentials",
  },
  {
    id: "done",
    title: "You're set",
    body: [
      "That's the tour. Everything above is optional and reversible, and the gates stay on regardless of what you connect.",
      "You can reopen this tour any time from Settings. Press the panic button to stop all future outward actions instantly.",
    ],
    route: null,
    actionLabel: null,
  },
] as const;

/** How many steps the wizard has (for the progress indicator + bounds). */
export const ONBOARDING_STEP_COUNT = ONBOARDING_STEPS.length;

/* --- the once-only persisted flag (DOM-free, fail-safe) ------------------- */

/** A minimal storage shape so the flag helpers can be unit-tested with an
 *  in-memory stub and degrade safely when localStorage is unavailable (a
 *  no-DOM/SSR/vitest-node context). */
export interface OnboardingStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

/** The real browser localStorage when present, else null. Never throws (a
 *  privacy-mode / sandboxed context can throw on access). */
function defaultStorage(): OnboardingStorage | null {
  try {
    if (typeof localStorage === "undefined") return null;
    return localStorage;
  } catch {
    return null;
  }
}

/** Has the user already seen (completed or skipped) onboarding?
 *
 *  FAIL-SAFE TOWARD NOT-SHOWING: if storage is unavailable OR the read throws,
 *  we return TRUE (treat as already-seen) so the wizard never gets stuck
 *  re-appearing every launch on a context where the flag can't persist. The
 *  whole point is "show ONCE"; a context that can't remember the dismissal
 *  should not nag. (First-run on a normal browser persists fine.) */
export function hasSeenOnboarding(storage: OnboardingStorage | null = defaultStorage()): boolean {
  if (storage === null) return true;
  try {
    return storage.getItem(ONBOARDING_SEEN_KEY) === ONBOARDING_SEEN_VALUE;
  } catch {
    return true;
  }
}

/** Persist that onboarding has been seen (completed OR skipped) so it never
 *  reappears on its own. Idempotent; never throws (a failed write just means the
 *  wizard may show again next launch — annoying, never unsafe). */
export function markOnboardingSeen(storage: OnboardingStorage | null = defaultStorage()): void {
  if (storage === null) return;
  try {
    storage.setItem(ONBOARDING_SEEN_KEY, ONBOARDING_SEEN_VALUE);
  } catch {
    /* a failed persist is non-fatal — see hasSeenOnboarding's fail-safe note */
  }
}

/** Clamp a step index into range (defensive — a Back/Next can never escape the
 *  deck). */
export function clampStep(index: number): number {
  if (!Number.isFinite(index) || index < 0) return 0;
  if (index >= ONBOARDING_STEP_COUNT) return ONBOARDING_STEP_COUNT - 1;
  return Math.floor(index);
}

/** Is `index` the last step? (drives Next vs Finish). */
export function isLastStep(index: number): boolean {
  return clampStep(index) === ONBOARDING_STEP_COUNT - 1;
}
