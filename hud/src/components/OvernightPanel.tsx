import type { OvernightStatus } from "../core/events";
import Frame from "./Frame";

/**
 * OVERNIGHT // ASYNC AGENTS — the honest state of the overnight task queue +
 * morning brief (daemon overnight.rs, F10).
 *
 * HONESTY CONTRACT (do not regress):
 *   - SHIPS OFF: OFF unless the operator enables [overnight]. The pill says so.
 *   - CLOUD-GATED: ARMED · NEEDS KEY until an Anthropic key exists, then READY.
 *   - TOOL-LESS: overnight work drafts, never acts — the footnote states, and
 *     `runsTools` pins, that no tools are ever passed, so nothing consequential
 *     can happen while you sleep.
 */
export default function OvernightPanel({ overnight }: { overnight: OvernightStatus | null }) {
  if (overnight === null) return null;

  const state = agentState(overnight);
  return (
    <div className="overnight-panel">
      <Frame title="OVERNIGHT // ASYNC AGENTS" tag="TOOL-LESS · DRAFTS ONLY">
        <div className="overnight-body">
          <div className="overnight-head">
            <span className={`overnight-pill ${state.cls}`}>{state.label}</span>
            <span className="overnight-count dim-note">
              {overnight.queued} queued · {overnight.done} done
              {overnight.failed > 0 ? ` · ${overnight.failed} failed` : ""}
            </span>
          </div>
          {overnight.items.length > 0 && (
            <ul className="overnight-items">
              {overnight.items.map((it, i) => (
                <li key={`${it.prompt}-${i}`} className={`overnight-item ${it.status}`}>
                  <span className="overnight-item-prompt">{it.prompt}</span>
                  {it.result && <span className="overnight-item-result dim-note">{it.result}</span>}
                </li>
              ))}
            </ul>
          )}
          <div className="overnight-foot dim-note">
            Runs your queued tasks while you&rsquo;re away and folds the results
            into a morning brief. Overnight agents are tool-less — they research
            and draft, but can never send, buy, or change anything; that waits for
            your confirmation.
          </div>
        </div>
      </Frame>
    </div>
  );
}

function agentState(o: OvernightStatus): { label: string; cls: string } {
  if (!o.enabled) return { label: "OFF", cls: "off" };
  if (!o.cloudKeyPresent) return { label: "ARMED · NEEDS KEY", cls: "armed" };
  return { label: "READY", cls: "ready" };
}
