import { useCallback, useState } from "react";
import { PERMISSION_PANES, PERMISSIONS_COPY } from "../core/permissions";
import {
  inTauri,
  openPrivacyPane,
  requestAccess,
  requestAllPermissions,
} from "../tauri/bridge";

/**
 * SYSTEM ACCESS // macOS PERMISSIONS — the panel that takes the user straight to
 * each macOS Privacy & Security switch JARVIS uses.
 *
 * HONESTY CONTRACT (do not regress): macOS does NOT let any app grant itself a
 * TCC permission. This panel can only OPEN the right pane (and the OS prompts on
 * first use); the user flips the switch. The copy says so plainly and the
 * buttons never claim a permission was granted — they reflect whether System
 * Settings actually opened. The backend is allowlist-guarded (a key, never a
 * URL), so a button can only ever open a known Privacy pane.
 */
export default function SystemAccessPanel() {
  const shell = inTauri();
  // Which control is mid-flight ("__all__" for the re-request-all button, else a
  // pane key). Disables every button while one is opening so a double-press can't
  // race two `open`s.
  const [busy, setBusy] = useState<string | null>(null);
  const [note, setNote] = useState("");

  const openPane = useCallback(
    async (key: string) => {
      if (busy) return;
      setBusy(key);
      setNote("");
      try {
        // Fire the NATIVE macOS prompt from the app (clean "JARVIS" dialog); the
        // backend falls back to opening Settings when macOS won't prompt.
        const r = await requestAccess(key);
        setNote(r.detail);
      } catch {
        setNote("could not request the permission");
      } finally {
        setBusy(null);
      }
    },
    [busy],
  );

  const openAll = useCallback(async () => {
    if (busy) return;
    setBusy("__all__");
    setNote("");
    try {
      const r = await requestAllPermissions();
      setNote(r.detail);
    } catch {
      setNote("could not open System Settings");
    } finally {
      setBusy(null);
    }
  }, [busy]);

  // Deep-link straight to a pane in System Settings WITHOUT firing a prompt —
  // handy to review or change an already-decided permission.
  const openSettings = useCallback(
    async (key: string) => {
      if (busy) return;
      setBusy(`settings:${key}`);
      setNote("");
      try {
        const r = await openPrivacyPane(key);
        setNote(r.detail);
      } catch {
        setNote("could not open System Settings");
      } finally {
        setBusy(null);
      }
    },
    [busy],
  );

  return (
    <div className="sysaccess">
      <div className="cred-section-title">{PERMISSIONS_COPY.title}</div>
      <div className="kv-note">{PERMISSIONS_COPY.lede}</div>

      <div className="cred-row">
        <div className="cred-label">All permissions</div>
        <button
          className="icon-btn sysaccess-all"
          onClick={() => void openAll()}
          disabled={!shell || busy !== null}
          title={PERMISSIONS_COPY.requestAllHint}
        >
          {busy === "__all__" ? "OPENING…" : PERMISSIONS_COPY.requestAll}
        </button>
        <div className="cred-hint">{PERMISSIONS_COPY.requestAllHint}</div>
      </div>

      <div className="cred-section-title">PERMISSIONS</div>
      {PERMISSION_PANES.map((p) => (
        <div className="cred-row" key={p.key}>
          <div className="cred-label">{p.label}</div>
          <button
            className="icon-btn"
            onClick={() => void openPane(p.key)}
            disabled={!shell || busy !== null}
            title={`Request “${p.label}” access — shows the macOS prompt`}
            aria-label={`Request ${p.label} access`}
          >
            {busy === p.key ? "REQUESTING…" : "REQUEST"}
          </button>
          <button
            className="icon-btn"
            onClick={() => void openSettings(p.key)}
            disabled={!shell || busy !== null}
            title={`Open “${p.label}” in System Settings (no prompt)`}
            aria-label={`Open ${p.label} in System Settings`}
          >
            {busy === `settings:${p.key}` ? "OPENING…" : "SETTINGS"}
          </button>
          <div className="cred-hint">{p.why}</div>
        </div>
      ))}

      <div className="kv-note">
        <b>{PERMISSIONS_COPY.howTitle}</b>
        <ul className="voiceid-keys">
          {PERMISSIONS_COPY.how.map((h) => (
            <li key={h}>{h}</li>
          ))}
        </ul>
      </div>

      <div className="kv-note">{PERMISSIONS_COPY.footnote}</div>

      {note ? (
        <div className="cred-hint" role="status" aria-live="polite">
          {note}
        </div>
      ) : null}

      {!shell ? (
        <div className="cred-hint">
          Opening System Settings needs the JARVIS desktop app — these buttons are
          inactive in a browser preview.
        </div>
      ) : null}
    </div>
  );
}
