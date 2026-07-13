import type { SyncStatus } from "../core/events";
import Frame from "./Frame";

/**
 * SYNC // FEDERATED MEMORY — the honest state of the E2E-encrypted cross-device
 * fact sync (daemon sync.rs).
 *
 * HONESTY CONTRACT (do not regress):
 *   - SHIPS OFF: OFF unless the operator enables [sync]. The pill says so.
 *   - E2E-ENCRYPTED: a bundle never leaves the box unsealed; the transport is
 *     armed-but-inert (moving a sealed bundle to a paired device is the
 *     device-gated leg). ARMED · NEEDS PAIRING until a shared key exists.
 *   - NEVER SILENTLY CLOBBERS: a divergence between devices is surfaced as a
 *     pending conflict, never an invisible overwrite.
 *   - HONEST SCOPE: deletions don't propagate (no tombstones) — the footnote
 *     says so rather than implying a full mirror.
 */
export default function SyncPanel({ sync }: { sync: SyncStatus | null }) {
  if (sync === null) return null;

  const state = pipelineState(sync);
  return (
    <div className="sync-panel">
      <Frame title="SYNC // FEDERATED MEMORY" tag="E2E · NEVER PLAINTEXT">
        <div className="sync-body">
          <div className="sync-head">
            <span className={`sync-pill ${state.cls}`}>{state.label}</span>
            <span className="sync-facts dim-note">
              {sync.syncableFacts} fact{sync.syncableFacts === 1 ? "" : "s"} syncable
            </span>
          </div>
          {sync.pendingConflicts > 0 && (
            <div className="sync-conflicts">
              {sync.pendingConflicts} conflict{sync.pendingConflicts === 1 ? "" : "s"} to
              resolve — a peer's value never overwrote yours silently
            </div>
          )}
          <div className="sync-foot dim-note">
            Your facts sync across your own devices, end-to-end encrypted (a
            bundle never leaves the box unsealed). The network transport is armed
            but inert. Deletions don&rsquo;t propagate yet.
          </div>
        </div>
      </Frame>
    </div>
  );
}

function pipelineState(s: SyncStatus): { label: string; cls: string } {
  if (!s.enabled) return { label: "OFF", cls: "off" };
  if (!s.keyPresent) return { label: "ARMED · NEEDS PAIRING", cls: "armed" };
  return { label: "ARMED · PAIRED", cls: "ready" };
}
