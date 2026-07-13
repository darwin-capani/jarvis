import type { JournalSnapshot } from "../core/events";
import Frame from "./Frame";

/**
 * JOURNAL // EXECUTED ACTIONS — the reversible-action ledger (daemon
 * journal.rs). One row per consequential action that ACTUALLY EXECUTED this
 * daemon session, each with its honest undo verdict: a green UNDOABLE pill when
 * an already-wired inverse exists (spoken "undo that" arms it through the same
 * confirm gate), a dim NO UNDO pill with the specific reason otherwise, and an
 * amber UNDONE pill once the inverse has executed.
 *
 * HONESTY: rows are only ever genuinely-executed actions (never dry-run
 * previews), the ledger is session-scoped (a daemon restart honestly starts it
 * empty), and `undoable` is never over-claimed — most consequential actions
 * (sent mail, public posts) are irreversible and say so.
 */
export default function JournalPanel({ journal }: { journal: JournalSnapshot | null }) {
  if (journal === null) return null;

  return (
    <div className="journal-panel">
      <Frame title="JOURNAL // EXECUTED ACTIONS" tag="SYSTEM">
        <div className="journal-body">
          {journal.entries.length === 0 ? (
            <div className="journal-empty dim-note">
              no consequential actions executed this session
            </div>
          ) : (
            <>
              <div className="journal-count dim-note">
                {journal.count} executed this session · say “undo that” to reverse the last one
              </div>
              <ul className="journal-list">
                {journal.entries.map((e, i) => (
                  <li key={`${e.ts}-${i}`} className="journal-entry">
                    <div className="journal-head">
                      <span className={`journal-pill ${pillClass(e.undone, e.undoable)}`}>
                        {pillLabel(e.undone, e.undoable)}
                      </span>
                      <span className="journal-tool">{e.tool}</span>
                      <span className="journal-meta dim-note">
                        {e.agent}
                        {e.via === "policy" ? " · auto-approved" : ""}
                      </span>
                    </div>
                    <div className="journal-preview">{e.preview}</div>
                    {e.note !== "" && <div className="journal-note dim-note">{e.note}</div>}
                  </li>
                ))}
              </ul>
            </>
          )}
        </div>
      </Frame>
    </div>
  );
}

function pillClass(undone: boolean, undoable: boolean): string {
  if (undone) return "undone";
  return undoable ? "undoable" : "no-undo";
}

function pillLabel(undone: boolean, undoable: boolean): string {
  if (undone) return "UNDONE";
  return undoable ? "UNDOABLE" : "NO UNDO";
}
