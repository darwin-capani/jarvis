import { useEffect, useReducer, useRef, useState } from "react";
import {
  applyReduce,
  CONFIDENCE_SEGMENTS,
  confidencePct,
  confirmReady,
  initialApplyState,
  litSegments,
  REARM_MS,
  stageLabel,
} from "../core/heal";
import type { HealDiagnosing, HealProposal } from "../core/state";
import { healApply, healProposalDetail } from "../tauri/bridge";
import Frame from "./Frame";

/**
 * SELF-REPAIR // PROPOSALS — the self-heal v2 review surface.
 *
 * Two warn-amber states (a pending proposal is an *attention* state, NOT an
 * error — so NO alert-red on the panel chrome; red lives only on the
 * rejected/blocked banner in AlertPanel, and on the in-panel apply-FAILED
 * notice):
 *   1. DIAGNOSING  — heal.diagnosing landed: root cause extracted, the cloud
 *                    drafter/validator/reviewer loop is running.
 *   2. PROPOSAL    — heal.proposal landed: a validated, review-scored patch is
 *                    staged and awaiting GATED human review.
 *
 * SAFETY CONTRACT (do not regress): the GUI Accept path is a HUMAN-GATED apply,
 * not auto-heal. The human reviews the ACTUAL diff (fetched + shown in a
 * scrollable block), then ACCEPT is TWO-STEP: the first click arms a distinct
 * "CONFIRM — APPLY & REBUILD" state, and only the second click (after a short
 * re-arm window so a double-click cannot skip the confirm) calls heal_apply,
 * which runs scripts/apply_heal.sh <ts> --yes — the SAME gates as the terminal
 * path (fresh staging copy + cargo check + full cargo test) and refuses to
 * touch daemon/src if validation fails. The read-only terminal command line is
 * kept too. self_heal still ships enabled=false.
 *
 * The proposal is rendered only when the daemon reports validated=true (the
 * only thing it ever emits as heal.proposal); a defensive guard below keeps it
 * that way even if a malformed event ever arrived.
 *
 * Anti-flicker: the parent only re-renders when the reducer produces a new
 * proposal/diagnosis reference (heal events are rare and discrete); the
 * confidence gauge is a static segmented bar; the apply lifecycle lives in a
 * LOCAL useReducer (the pure machine in core/heal.ts) so an in-flight apply
 * never churns the global HUD tree.
 */

function ConfidenceGauge({ confidence }: { confidence: number | null }) {
  if (confidence === null) {
    return (
      <div className="sh-conf">
        <span className="sh-conf-label">REVIEW CONFIDENCE</span>
        <span className="sh-conf-na dim-note">n/a (older daemon)</span>
      </div>
    );
  }
  const lit = litSegments(confidence);
  const pct = confidencePct(confidence);
  return (
    <div className="sh-conf">
      <span className="sh-conf-label">REVIEW CONFIDENCE</span>
      <span className="sh-conf-gauge" aria-hidden="true">
        {Array.from({ length: CONFIDENCE_SEGMENTS }, (_, i) => (
          <i key={i} className={i < lit ? "on" : ""} />
        ))}
      </span>
      <span className="sh-conf-pct">{pct}%</span>
    </div>
  );
}

function Diagnosing({ diag }: { diag: HealDiagnosing }) {
  return (
    <div className="sh-body">
      <div className="sh-state-line">
        <span className="sh-spinner" aria-hidden="true" />
        <span className="sh-state-label">DIAGNOSING ROOT CAUSE</span>
      </div>
      {diag.subsystem ? (
        <div className="sh-row">
          <span className="sh-k">SUBSYSTEM</span>
          <span className="sh-v">{diag.subsystem}</span>
        </div>
      ) : null}
      {diag.signature ? (
        <div className="sh-row">
          <span className="sh-k">SIGNATURE</span>
          <span className="sh-v sh-sig">{diag.signature}</span>
        </div>
      ) : null}
      {diag.files.length > 0 ? (
        <div className="sh-row">
          <span className="sh-k">CITED</span>
          <span className="sh-v">{diag.files.join(", ")}</span>
        </div>
      ) : null}
      <div className="sh-note dim-note">
        Drafting candidate patches, staging + validating each, then
        adversarially reviewing the survivors…
      </div>
    </div>
  );
}

/** The fetched-diff review block. Shows a loading note, then the real diff in a
 *  scrollable monospace pane (the human reviews the actual code change), plus
 *  report highlights when present. In a plain browser the bridge returns an
 *  empty diff + a marker message, which this renders as a "desktop app" note. */
function DiffReview({ ts }: { ts: number }) {
  const [diff, setDiff] = useState<string | null>(null);
  const [report, setReport] = useState<string>("");
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setDiff(null);
    setErr(null);
    healProposalDetail(String(ts))
      .then((d) => {
        if (!live) return;
        setDiff(d.diff);
        setReport(d.report);
      })
      .catch((e: unknown) => {
        if (!live) return;
        setErr(e instanceof Error ? e.message : String(e));
      });
    return () => {
      live = false;
    };
  }, [ts]);

  if (err) {
    return (
      <div className="sh-note dim-note">
        Could not load the staged diff: {err}
      </div>
    );
  }
  if (diff === null) {
    return <div className="sh-note dim-note">Loading staged diff…</div>;
  }
  if (diff.trim() === "") {
    // Browser dev (no shell) or a genuinely empty patch — show whatever the
    // report marker said rather than an empty box.
    return (
      <div className="sh-note dim-note">
        {report || "(no diff to display)"}
      </div>
    );
  }

  // First report.md line(s) as highlights, when present.
  const reportHead = report
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean)
    .slice(0, 3)
    .join("  ·  ");

  return (
    <>
      {reportHead ? (
        <div className="sh-report-head dim-note">{reportHead}</div>
      ) : null}
      <pre className="sh-diff" tabIndex={0} aria-label="staged diff for review">
        {diff}
      </pre>
    </>
  );
}

function Proposal({
  proposal,
  onDismiss,
}: {
  proposal: HealProposal;
  onDismiss: () => void;
}) {
  // The apply lifecycle is a LOCAL pure-reducer state machine (core/heal.ts):
  // idle -> confirming -> applying -> applied|failed, with a two-step confirm
  // and a re-arm guard.
  const [apply, dispatch] = useReducer(applyReduce, undefined, initialApplyState);
  // Force a re-render when the re-arm window elapses so the CONFIRM button
  // flips from disabled to enabled without needing another event.
  const [, bump] = useState(0);
  const rearmTimer = useRef<number | null>(null);

  useEffect(() => {
    return () => {
      if (rearmTimer.current !== null) window.clearTimeout(rearmTimer.current);
    };
  }, []);

  // Defensive: heal.proposal is only emitted for validated patches. If a
  // non-validated one ever arrived, do NOT present it as ready-to-apply.
  if (!proposal.validated) {
    return (
      <div className="sh-body">
        <div className="sh-state-line">
          <span className="sh-state-label">PROPOSAL NOT VALIDATED — HELD</span>
        </div>
        <div className="sh-note dim-note">
          A proposal arrived without passing the validation gates and will not
          be surfaced for apply.
        </div>
      </div>
    );
  }

  const ts = proposal.refTs;
  const applyCmd = ts !== null ? `scripts/apply_heal.sh ${ts}` : null;

  function onAccept() {
    const now = Date.now();
    dispatch({ type: "accept", at: now });
    // Arm a timer so the CONFIRM button auto-enables once REARM_MS passes.
    if (rearmTimer.current !== null) window.clearTimeout(rearmTimer.current);
    rearmTimer.current = window.setTimeout(() => bump((n) => n + 1), REARM_MS + 20);
  }

  function onConfirm() {
    const now = Date.now();
    // Enforce the re-arm guard at the click site too (defense in depth — the
    // reducer also enforces it).
    if (!confirmReady(apply, now)) return;
    if (ts === null) return;
    dispatch({ type: "confirm", at: now });
    // Spawn the gated apply. The reducer already moved us to `applying`.
    void healApply(String(ts)).then(
      (res) => {
        if (!res.available) {
          dispatch({
            type: "applyFail",
            message:
              "Accept & apply is available in the desktop app. From a terminal, run the command shown above.",
          });
          return;
        }
        if (res.ok) {
          // The script prints whether the daemon was kickstarted.
          const restarted = /daemon restarted/i.test(res.log);
          dispatch({
            type: "applyOk",
            restarted,
            message: restarted
              ? "Healed. DARWIN restarted on the new build."
              : "Healed. Restart darwind to run the healed build.",
          });
        } else {
          dispatch({
            type: "applyFail",
            message: `Validation/apply failed (${res.stage}). Patch NOT applied — live code untouched.`,
          });
        }
      },
      (e: unknown) => {
        dispatch({
          type: "applyFail",
          message: `Apply could not run: ${
            e instanceof Error ? e.message : String(e)
          }. Patch NOT applied.`,
        });
      },
    );
  }

  const now = Date.now();
  const ready = confirmReady(apply, now);
  const dismissable = apply.phase !== "applying";

  return (
    <div className="sh-body">
      <div className="sh-state-line">
        <span className="sh-state-label">PROPOSAL READY FOR REVIEW</span>
      </div>

      {proposal.subsystem ? (
        <div className="sh-row">
          <span className="sh-k">SUBSYSTEM</span>
          <span className="sh-v">{proposal.subsystem}</span>
        </div>
      ) : null}
      {proposal.signature ? (
        <div className="sh-row">
          <span className="sh-k">SIGNATURE</span>
          <span className="sh-v sh-sig">{proposal.signature}</span>
        </div>
      ) : null}
      <div className="sh-row">
        <span className="sh-k">
          FILE{proposal.files.length === 1 ? "" : "S"}
        </span>
        <span className="sh-v">
          {proposal.files.length > 0 ? proposal.files.join(", ") : "—"}
        </span>
      </div>
      <div className="sh-row">
        <span className="sh-k">VALIDATION</span>
        <span className="sh-v sh-pass">PASSED — cargo check + full cargo test</span>
      </div>

      <ConfidenceGauge confidence={proposal.confidence} />

      {/* The actual code change, for HUMAN REVIEW. */}
      {ts !== null ? (
        <div className="sh-review">
          <div className="sh-review-label">REVIEW THE STAGED DIFF</div>
          <DiffReview ts={ts} />
        </div>
      ) : (
        <div className="sh-note dim-note">
          (proposal timestamp missing — see state/heal/proposals/)
        </div>
      )}

      {/* Read-only terminal path — kept alongside the GUI Accept. */}
      {applyCmd ? (
        <div className="sh-cmd" role="note">
          <span className="sh-cmd-prompt" aria-hidden="true">
            $
          </span>
          <code>{applyCmd}</code>
        </div>
      ) : null}

      <div className="sh-safety dim-note">
        Accepting re-validates the patch (cargo check + full test) and will not
        apply if validation fails.
      </div>

      {/* Apply lifecycle status line. */}
      {apply.phase === "applying" ? (
        <div className="sh-apply-status applying">
          <span className="sh-spinner" aria-hidden="true" />
          <span>{stageLabel(apply.stage)}</span>
        </div>
      ) : apply.phase === "applied" ? (
        <div className="sh-apply-status applied">{apply.message}</div>
      ) : apply.phase === "failed" ? (
        <div className="sh-apply-status failed" role="alert">
          {apply.message}
        </div>
      ) : null}

      <div className="sh-foot">
        <span className="sh-foot-hint dim-note">
          report.md, diagnosis.json, candidates.md + review.md written under the
          proposal directory
        </span>
        <div className="sh-actions">
          <button
            className="sh-ack"
            onClick={onDismiss}
            disabled={!dismissable}
            title={dismissable ? undefined : "cannot dismiss mid-apply"}
          >
            DISMISS
          </button>

          {apply.phase === "idle" ? (
            <button
              className="sh-apply"
              onClick={onAccept}
              disabled={ts === null}
            >
              ACCEPT &amp; APPLY
            </button>
          ) : apply.phase === "confirming" ? (
            <button
              className="sh-apply confirm"
              onClick={onConfirm}
              disabled={!ready}
              title={ready ? undefined : "confirm arms shortly…"}
            >
              CONFIRM — APPLY &amp; REBUILD
            </button>
          ) : apply.phase === "failed" || apply.phase === "applied" ? (
            <button
              className="sh-apply"
              onClick={() => dispatch({ type: "reset" })}
            >
              {apply.phase === "applied" ? "CLOSE" : "BACK"}
            </button>
          ) : (
            <button className="sh-apply" disabled>
              APPLYING…
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

export default function SelfHealPanel({
  diagnosing,
  proposal,
  onDismiss,
}: {
  diagnosing: HealDiagnosing | null;
  proposal: HealProposal | null;
  onDismiss: () => void;
}) {
  // A pending proposal takes precedence over a stale diagnosis (the reducer
  // already clears diagnosing on proposal, but guard the render order too).
  if (!proposal && !diagnosing) return null;

  const tag = proposal ? "PENDING REVIEW" : "DIAGNOSING";

  return (
    <div className="self-heal-panel">
      <Frame className="self-heal attn" title="SELF-REPAIR // PROPOSALS" tag={tag}>
        {proposal ? (
          // Key by proposal identity so a NEW proposal remounts <Proposal> with a
          // fresh apply state machine. Without it, when the reducer swaps one
          // proposal for another directly (two heal.proposal frames with no
          // intervening diagnosing/dismiss null), React reuses the instance and the
          // brand-new proposal inherits the prior one's terminal "applied"/"failed"
          // apply UI — reading as already-healed and blocking APPLY.
          <Proposal key={proposal.refTs ?? "no-ref"} proposal={proposal} onDismiss={onDismiss} />
        ) : diagnosing ? (
          <Diagnosing diag={diagnosing} />
        ) : null}
      </Frame>
    </div>
  );
}
