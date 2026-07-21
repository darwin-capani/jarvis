import { useCallback, useEffect, useRef, useState } from "react";
import Frame from "./Frame";
import useModalFocus from "./useModalFocus";
import {
  ONBOARDING_STEPS,
  ONBOARDING_STEP_COUNT,
  clampStep,
  isLastStep,
  type OnboardingRouteTarget,
} from "../core/onboarding";

/**
 * FIRST-RUN ONBOARDING WIZARD (WS4b item 1).
 *
 * A multi-step modal shown ONCE on first run (App.tsx gates it on the real
 * persisted flag — hasSeenOnboarding/markOnboardingSeen). It honestly explains
 * the shipped ARMED-BUT-GATED posture and then ROUTES the user to the EXISTING,
 * already-gated surfaces — it never duplicates or bypasses them:
 *   - "settings-system"      -> opens SettingsModal on the SYSTEM SETTINGS tab
 *     (TCC guidance + the file-search/code RAG-roots editors + voice controls).
 *   - "settings-credentials" -> opens SettingsModal on the CREDENTIALS tab
 *     (the cloud key + OAuth connect + MCP tokens + voice-id review).
 *
 * SAFETY CONTRACT (do not regress):
 *   - It adds NO write/act path and touches NO gate code. The routing callback
 *     only OPENS an existing panel on a tab; the user still performs the gated
 *     enrolment / permission / credential step there.
 *   - "Skip / I'll do this later" is available on EVERY step. Skipping, finishing,
 *     OR routing away all DISMISS the wizard and set the seen flag (App owns the
 *     persistence via onDismiss), so it never reappears on its own.
 *   - It NEVER fabricates state — the copy describes the shipped defaults, not a
 *     live measurement.
 */
export default function OnboardingWizard({
  onRoute,
  onDismiss,
}: {
  /** Open an existing surface (App routes a non-null target to SettingsModal on
   *  the matching tab). Routing also dismisses the wizard. */
  onRoute: (target: Exclude<OnboardingRouteTarget, null>) => void;
  /** Dismiss the wizard (Skip / Finish / Esc / backdrop). App persists the seen
   *  flag here so the wizard never reappears. */
  onDismiss: () => void;
}) {
  const [index, setIndex] = useState(0);
  const step = ONBOARDING_STEPS[clampStep(index)];
  const last = isLastStep(index);

  // Esc dismisses (treated as Skip — the seen flag is set by App's onDismiss).
  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") onDismiss();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onDismiss]);

  const back = useCallback(() => setIndex((i) => clampStep(i - 1)), []);
  const next = useCallback(() => setIndex((i) => clampStep(i + 1)), []);

  // a11y: trap + autofocus + focus-restore (Escape stays on the window
  // listener above — wiring both would double-dismiss).
  const modalRef = useRef<HTMLDivElement>(null);
  useModalFocus(modalRef);

  // Primary action for a routing step: open the existing surface AND dismiss the
  // wizard (App opens SettingsModal on the right tab + sets the seen flag).
  const route = useCallback(() => {
    if (step.route !== null) onRoute(step.route);
  }, [step.route, onRoute]);

  return (
    <div className="modal-backdrop" onClick={onDismiss}>
      <div
        className="modal onboarding-modal"
        role="dialog"
        aria-modal="true"
        aria-label="DARWIN first-run setup"
        onClick={(e) => e.stopPropagation()}
        ref={modalRef}
      >
        <Frame title="WELCOME // FIRST-RUN SETUP" tag="ARMED · GATED">
          <div className="onboarding-body">
            {/* Progress: which step of how many (real count, never faked). */}
            <div className="onboarding-progress" aria-hidden="true">
              {ONBOARDING_STEPS.map((s, i) => (
                <span
                  key={s.id}
                  className={`onboarding-dot${i === clampStep(index) ? " active" : ""}${
                    i < clampStep(index) ? " done" : ""
                  }`}
                />
              ))}
            </div>
            <div className="onboarding-step-count dim-note">
              Step {clampStep(index) + 1} of {ONBOARDING_STEP_COUNT}
            </div>

            <h2 className="onboarding-title">{step.title}</h2>
            {step.body.map((para, i) => (
              <p key={i} className="onboarding-para">
                {para}
              </p>
            ))}

            {/* Primary action: route to the existing surface (routing steps only). */}
            {step.route !== null && step.actionLabel !== null && (
              <div className="onboarding-route">
                <button type="button" className="icon-btn onboarding-route-btn" onClick={route}>
                  {step.actionLabel} →
                </button>
                <span className="onboarding-route-note dim-note">
                  Opens the existing setup panel — you complete the step there. Nothing is changed or granted for you.
                </span>
              </div>
            )}

            {/* Nav: Skip is ALWAYS available; Back when not first; Next/Finish. */}
            <div className="onboarding-nav">
              <button
                type="button"
                className="icon-btn onboarding-skip"
                onClick={onDismiss}
                title="Dismiss the tour — you can reopen it from Settings"
              >
                Skip / I&apos;ll do this later
              </button>
              <div className="onboarding-nav-right">
                {clampStep(index) > 0 && (
                  <button type="button" className="icon-btn" onClick={back}>
                    Back
                  </button>
                )}
                {last ? (
                  <button type="button" className="icon-btn onboarding-finish" onClick={onDismiss}>
                    Finish
                  </button>
                ) : (
                  <button type="button" className="icon-btn onboarding-next" onClick={next}>
                    Next
                  </button>
                )}
              </div>
            </div>
          </div>
        </Frame>
      </div>
    </div>
  );
}
