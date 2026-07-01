import type { TccSentinel } from "../core/events";
import Frame from "./Frame";

/**
 * TCC // PERMISSION SENTINEL — the ambient READ-ONLY view of which apps hold
 * macOS privacy grants, fed by the tcc.snapshot / tcc.anomaly telemetry from
 * daemon/src/tcc.rs. It surfaces the LOCAL-PERMISSION vector (orthogonal to the
 * network Egress Sentinel and the system-state posture panel): the total grant
 * count, how many HIGH-RISK grants are currently ALLOWED (an app that can watch
 * your screen, log keystrokes, or read every file), and any recent new-grant /
 * denied→allowed escalation the sentinel detected against its baseline.
 *
 * SAFETY CONTRACT (do not regress):
 *   - REVIEW-ONLY. Nothing here revokes a grant, blocks an app, or changes any
 *     permission — it only SHOWS the surface so the user can spot a suspicious one.
 *   - SECRET-FREE. The wire carries only app/bundle ids + counts, never a token.
 *   - HONEST. When macOS blocks the read (Full Disk Access), the sentinel reports
 *     available=false and this panel says exactly that — it never fabricates an
 *     inventory or shows a stale one (parseTccSnapshot never returns null).
 */
export default function TccSentinelPanel({
  sentinel,
  anomalies,
}: {
  sentinel: TccSentinel | null;
  anomalies: string[];
}) {
  // No scan yet (the sentinel has not emitted) — render nothing, mirroring the
  // other event-fed review panels.
  if (sentinel === null) return null;

  return (
    <div className="tcc-panel">
      <Frame title="TCC // PERMISSION SENTINEL" tag="REVIEW ONLY">
        <div className="tcc-body">
          {!sentinel.available ? (
            <div className="tcc-note dim-note">
              macOS is blocking the privacy database (TCC). Grant JARVIS Full Disk
              Access in System Settings › Privacy &amp; Security to inventory app
              permissions — read-only; I never change a permission.
            </div>
          ) : (
            <>
              <div className="tcc-summary">
                <span className="tcc-count">
                  <span className="tcc-grants-n">{sentinel.grants}</span>
                  <span className="tcc-count-label"> GRANTS</span>
                </span>
                <span
                  className={`tcc-hr ${sentinel.highRiskAllowed > 0 ? "warn" : "ok"}`}
                >
                  {sentinel.highRiskAllowed} HIGH-RISK ALLOWED
                </span>
              </div>
              <div className="tcc-note dim-note">
                App privacy grants (Mic / Camera / Screen Recording / Accessibility
                / Full Disk Access …). High-risk = an app that can watch your
                screen, log keystrokes, or read every file. Review-only.
              </div>
              {anomalies.length > 0 && (
                <div className="tcc-anomalies">
                  <div className="tcc-anomalies-title">RECENT CHANGES</div>
                  {anomalies.map((a) => (
                    <div
                      key={a}
                      className={`tcc-anomaly ${a.includes("HIGH-RISK") ? "warn" : ""}`}
                      title={a}
                    >
                      {a}
                    </div>
                  ))}
                </div>
              )}
            </>
          )}
        </div>
      </Frame>
    </div>
  );
}
