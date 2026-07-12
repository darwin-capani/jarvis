import type { IntrospectStatus, IntrospectCapability } from "../core/events";
import Frame from "./Frame";

/**
 * INTROSPECT // MICRO-APP SENTINEL — the ambient READ-ONLY view of jarvisd's own
 * sandboxed micro-apps, fed by the introspect.snapshot / introspect.profile_drift
 * / introspect.anomaly / introspect.module_violation / introspect.modattest
 * telemetry from daemon/src/introspect.rs. It surfaces the SANDBOX-INTEGRITY
 * vector (orthogonal to the TCC permission sentinel and the network egress
 * panel): how many apps are observed, whether any seatbelt profile was tampered
 * on disk since launch, RSS/CPU runaways, any dyld module an app loaded that its
 * baseline never had (injection / unexpected dlopen), and the per-app
 * dyld-attestation summary (baseline seeded / unexpected+missing counts).
 *
 * SAFETY CONTRACT (do not regress):
 *   - REVIEW-ONLY. Nothing here kills an app, unloads a module, or rewrites a
 *     profile — it only SHOWS the surface so the user can spot a compromise.
 *   - SECRET-FREE. The wire carries app names + counts + module paths only, never
 *     a token, file contents, or the profile text (only a drift signal).
 *   - HONEST. The status never returns null (parseIntrospectSnapshot); the panel
 *     renders nothing only before the first tick, mirroring the other event-fed
 *     review panels.
 */
export default function IntrospectPanel({
  status,
  alerts,
  capabilities,
}: {
  status: IntrospectStatus | null;
  alerts: string[];
  capabilities: IntrospectCapability[];
}) {
  // No tick yet (the sentinel has not emitted) — render nothing.
  if (status === null) return null;

  const clean = status.drift === 0 && status.anomalies === 0 && alerts.length === 0;

  return (
    <div className="introspect-panel">
      <Frame title="INTROSPECT // MICRO-APP SENTINEL" tag="REVIEW ONLY">
        <div className="introspect-body">
          <div className="introspect-summary">
            <span className="introspect-count">
              <span className="introspect-apps-n">{status.apps}</span>
              <span className="introspect-count-label"> APPS OBSERVED</span>
            </span>
            <span className={`introspect-flags ${clean ? "ok" : "warn"}`}>
              {status.drift} DRIFT · {status.anomalies} ANOMALIES
            </span>
          </div>
          <div className="introspect-note dim-note">
            Read-only watch over JARVIS's own sandboxed micro-apps: SBPL profile
            tamper, runaway RSS/CPU, and unexpected loaded modules (dyld). It
            reports; it never kills, unloads, or changes a profile.
          </div>
          {alerts.length > 0 && (
            <div className="introspect-alerts">
              <div className="introspect-alerts-title">RECENT FINDINGS</div>
              {alerts.map((a) => (
                <div
                  key={a}
                  className={`introspect-alert ${
                    a.startsWith("MODULE") || a.startsWith("PROFILE") || a.startsWith("SECURITY")
                      ? "warn"
                      : ""
                  }`}
                  title={a}
                >
                  {a}
                </div>
              ))}
            </div>
          )}
          {capabilities.length > 0 && (
            <div className="introspect-caps">
              <div className="introspect-caps-title">DECLARED CAPABILITIES</div>
              {capabilities.map((c) => (
                <div className="introspect-cap" key={c.name}>
                  <span className="introspect-cap-name">{c.name}</span>
                  <span className="introspect-cap-list">{c.caps}</span>
                </div>
              ))}
            </div>
          )}
        </div>
      </Frame>
    </div>
  );
}
