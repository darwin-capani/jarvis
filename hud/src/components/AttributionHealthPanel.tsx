import type { AttributionHealth } from "../core/events";
import Frame from "./Frame";

/**
 * CAPABILITY // HEALTH — the PROPOSE-ONLY ambient view of how DARWIN's own
 * agents and skills are performing, computed from the outcome-labelled trace
 * corpus (daemon/src/attribution.rs, attribution.health). It surfaces how many
 * well-sampled capabilities are reliable vs failing, and NAMES the failing ones
 * so the user can decide to fix or disable them.
 *
 * SAFETY CONTRACT (do not regress):
 *   - PROPOSE-ONLY. Nothing here disables, promotes, or reroutes a capability —
 *     it only FLAGS what the evidence says so the user decides. (Auto-promotion /
 *     roster reweighting is a separate, gated follow-on.)
 *   - HONEST. Only WELL-SAMPLED capabilities are judged (the daemon excludes
 *     low-sample ones), and parseAttributionHealth never returns null — a
 *     malformed/empty payload reads as an honest all-zero snapshot, not a stale one.
 *   - SECRET-FREE. The wire carries capability names + counts only.
 */
export default function AttributionHealthPanel({
  health,
}: {
  health: AttributionHealth | null;
}) {
  // No snapshot yet (the sentinel has not emitted) — render nothing.
  if (health === null) return null;

  return (
    <div className="attr-panel">
      <Frame title="CAPABILITY // HEALTH" tag="REVIEW ONLY">
        <div className="attr-body">
          <div className="attr-summary">
            <span className="attr-stat">
              <span className="attr-ok-n">{health.reliable}</span>
              <span className="attr-stat-label"> RELIABLE</span>
            </span>
            <span className="attr-stat">
              <span className="attr-mixed-n">{health.mixed}</span>
              <span className="attr-stat-label"> MIXED</span>
            </span>
            <span className={`attr-stat ${health.failing > 0 ? "warn" : ""}`}>
              <span className="attr-fail-n">{health.failing}</span>
              <span className="attr-stat-label"> FAILING</span>
            </span>
            <span className="attr-turns">{health.turns} turns</span>
          </div>
          {health.flags.length > 0 ? (
            <div className="attr-flags">
              <div className="attr-flags-title">NEEDS ATTENTION</div>
              {health.flags.map((f) => (
                <div className="attr-flag" key={`${f.kind}:${f.name}`}>
                  <span className="attr-flag-name">{f.name}</span>
                  <span className="attr-flag-kind">{f.kind}</span>
                  <span className="attr-flag-stat">
                    {f.turns} turns · {f.rate}% success
                  </span>
                </div>
              ))}
            </div>
          ) : (
            <div className="attr-note dim-note">
              No well-sampled capability is failing. Rankings are evidence-based —
              low-sample agents/skills are not judged. Review-only.
            </div>
          )}
          {health.promote.length > 0 && (
            <div className="attr-promote">
              <div className="attr-promote-title">READY TO PROMOTE</div>
              {health.promote.map((f) => (
                <div className="attr-flag" key={`promote:${f.name}`}>
                  <span className="attr-promote-name">{f.name}</span>
                  <span className="attr-flag-kind">skill</span>
                  <span className="attr-flag-stat">
                    {f.turns} turns · {f.rate}% success
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>
      </Frame>
    </div>
  );
}
