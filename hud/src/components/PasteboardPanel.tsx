import type { PasteboardStatus } from "../core/events";
import Frame from "./Frame";

/**
 * SEMANTIC PASTEBOARD (daemon pasteboard.rs). Recall-by-MEANING over the macOS
 * clipboard, on-device. SHIPS OFF (opt-in): capturing the clipboard is
 * privacy-sensitive, so until the user enables it nothing is polled or stored and
 * the panel says so honestly.
 *
 * PRIVACY: every clip shown here was PII-REDACTED at capture (emails / secrets /
 * card + phone numbers stripped) and truncated to a short preview — the panel
 * never shows raw clip text, and when the pasteboard is off it shows nothing
 * captured. The ring is bounded + transient (in-RAM only, off disk / memory /
 * the optimizer).
 */
export default function PasteboardPanel({ pasteboard }: { pasteboard: PasteboardStatus | null }) {
  if (pasteboard === null) return null;

  return (
    <div className="pasteboard-panel">
      <Frame title="SEMANTIC PASTEBOARD" tag="MNEMOSYNE">
        <div className="pasteboard-body">
          <div className="pasteboard-head">
            <span className={`pasteboard-pill ${pasteboard.enabled ? "on" : "off"}`}>
              {pasteboard.enabled ? "CAPTURING" : "OFF"}
            </span>
            {pasteboard.enabled ? (
              <span className="pasteboard-meta dim-note">
                {pasteboard.count} / {pasteboard.cap} clips · every {pasteboard.pollIntervalSecs}s
              </span>
            ) : (
              <span className="pasteboard-meta dim-note">
                opt-in — enable [pasteboard] to recall what you copy
              </span>
            )}
          </div>
          {pasteboard.enabled && (
            <ul className="pasteboard-clips">
              {pasteboard.recent.length === 0 ? (
                <li className="pasteboard-empty dim-note">nothing copied yet</li>
              ) : (
                pasteboard.recent.map((clip, i) => (
                  <li key={i} className="pasteboard-clip">
                    {clip}
                  </li>
                ))
              )}
            </ul>
          )}
        </div>
      </Frame>
    </div>
  );
}
