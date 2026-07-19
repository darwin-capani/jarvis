import type { InferencePerfStatus } from "../core/events";
import {
  quantHonest,
  quantIsKnown,
  quantLabel,
  speculativeHonest,
  speculativeLabel,
  speculativeTone,
  throttleHonest,
  throttleReasonLabel,
  throttleTierPrefLabel,
  throttleTone,
} from "../core/events";
import Frame from "./Frame";

/**
 * INFERENCE // PERF — the read-only surface for the three perf seams:
 *   - SPECULATIVE DECODING (#37): on/off + whether it ACTUALLY ran this turn
 *     (the honest "draft model not available -> normal generation" fall back),
 *     plus a device-gated-speedup note.
 *   - THROTTLE (#38): the current ThrottlePlan (reason + preferred Local sub-tier
 *     + defer-heavy), or the honest "no throttle" resting state — with the note
 *     that the live power read is device-gated.
 *   - QUANTIZATION (#39): the quant that ACTUALLY loaded (not merely the one
 *     requested) + the honest "fell back to X" reporting.
 *
 * HONESTY CONTRACT (do not regress):
 *   - REPORTS THE PATH THAT ACTUALLY RAN, NEVER A FABRICATED PERF NUMBER. The
 *     fields are sourced from the daemon's per-turn `model.tier` payload: the
 *     server returns `speculative` (the path that really drove the decode) and
 *     `quant` (the quant that really loaded), and the daemon emits `throttle`
 *     ONLY when the plan really throttled. The panel surfaces those verbatim.
 *   - DEVICE-GATED EFFECTS ARE NEVER MEASURED HERE. The real speedup
 *     (speculative), RAM/quality tradeoff (quant), and thermal/battery effect
 *     (throttle) are device/model-dependent; the copy says so and quotes no
 *     number it cannot honestly claim.
 *   - OFF/NEUTRAL DEFAULTS READ AS TODAY'S RUNTIME. Speculative OFF = normal
 *     generation; quant AUTO = loaded as configured; no throttle = the OFF
 *     [power].adaptive default (the live power read is device-gated). No phantom
 *     throttle indicator is ever shown.
 *   - READ ONLY. No button — this panel only SHOWS the per-turn inference facts
 *     the daemon already reported.
 *
 * The reducer seeds `inferencePerf` with the honest awaiting/no-throttle resting
 * state and folds the per-turn `model.tier` facts into it, so this component can
 * trust the surface it is handed. It renders nothing until the first answered
 * turn reports a speculative/quant fact OR a throttle is active (mirroring the
 * other event-fed panels — no placeholder before there is anything to show).
 */
export default function InferencePerfPanel({
  perf,
}: {
  perf: InferencePerfStatus;
}) {
  // Nothing to show until the daemon has reported a per-turn inference fact (a
  // speculative/quant readout) or an active throttle. A fresh daemon (or an old
  // server that sends neither) leaves all three null/none — render nothing
  // rather than an empty placeholder.
  const hasReadout =
    perf.speculative !== null ||
    perf.quant !== null ||
    perf.throttle !== null ||
    perf.tps !== null ||
    perf.peakMemGb !== null;
  if (!hasReadout) return null;

  return (
    <div className="infperf-panel">
      <Frame title="INFERENCE // PERF" tag="ACTUAL PATH · READ ONLY">
        <div className="infperf-body">
          <DecodeRow perf={perf} />
          <SpeculativeRow perf={perf} />
          <ThrottleRow perf={perf} />
          <QuantRow perf={perf} />

          <div className="infperf-foot dim-note">
            DECODE is the throughput + peak memory mlx_lm actually MEASURED for the
            last on-device turn (real numbers, never estimated — blank when a path
            reported none). The rest report the path that ran: the device-gated
            speedup (speculative), RAM/quality tradeoff (quant), and thermal/battery
            effect (throttle) are not measured here.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** DECODE — the mlx_lm-MEASURED decode throughput (tok/s) + peak GPU memory
 *  (GB, decimal — the server's mx.get_peak_memory / 1e9, NOT binary GiB) of the
 *  last on-device turn that reported them. A real measurement, not
 *  a device-gated estimate: renders nothing until a measured turn arrives, and
 *  each figure independently blanks when the server reported none (the
 *  speculative/uncached single-shot paths measure nothing). */
function DecodeRow({ perf }: { perf: InferencePerfStatus }) {
  if (perf.tps === null && perf.peakMemGb === null) return null;
  return (
    <div className="infperf-row">
      <div className="infperf-row-head">
        <span className="infperf-title">DECODE</span>
        <span className="dot ok" aria-hidden="true" />
        {perf.tps !== null && (
          <span
            className="infperf-pill decode-tps"
            title="decode throughput mlx_lm measured on the last on-device turn (tokens/sec) — a real measurement, not an estimate"
          >
            {perf.tps.toFixed(1)} tok/s
          </span>
        )}
        {perf.peakMemGb !== null && (
          <span
            className="infperf-sub"
            title="peak GPU memory for the last measured turn (mx.get_peak_memory)"
          >
            {perf.peakMemGb.toFixed(2)} GB peak
          </span>
        )}
      </div>
      <div className="infperf-note dim-note">
        Measured by mlx_lm on the turn that ran — the honest on-device throughput,
        not a device-gated estimate.
      </div>
    </div>
  );
}

/** SPECULATIVE DECODING — whether speculative generation ACTUALLY drove the
 *  decode this turn, with the honest fallback note. Never the mere config flag. */
function SpeculativeRow({ perf }: { perf: InferencePerfStatus }) {
  const label = speculativeLabel(perf);
  const tone = speculativeTone(perf);
  return (
    <div className="infperf-row">
      <div className="infperf-row-head">
        <span className="infperf-title">SPECULATIVE DECODING</span>
        <span className={`dot ${tone === "good" ? "good" : "off"}`} aria-hidden="true" />
        <span
          className={`infperf-pill spec-${perf.speculative === true ? "on" : "off"}`}
          title="whether speculative decoding ACTUALLY ran this turn — the path that ran, not the config flag"
        >
          {label}
        </span>
      </div>
      <div className="infperf-note dim-note">{speculativeHonest(perf)}</div>
    </div>
  );
}

/** THROTTLE — the current battery/thermal throttle plan, or the honest "no
 *  throttle" resting state (the OFF [power].adaptive default). */
function ThrottleRow({ perf }: { perf: InferencePerfStatus }) {
  const plan = perf.throttle;
  const tone = throttleTone(plan);
  return (
    <div className="infperf-row">
      <div className="infperf-row-head">
        <span className="infperf-title">THROTTLE</span>
        <span className={`dot ${tone === "warn" ? "warn" : "off"}`} aria-hidden="true" />
        {plan === null ? (
          <span
            className="infperf-pill throttle-none"
            title="no throttle — the plan is neutral (the OFF default; the live power read is device-gated)"
          >
            NONE
          </span>
        ) : (
          <>
            <span
              className="infperf-pill throttle-on"
              title="why the plan throttled this turn — battery or thermal pressure"
            >
              {throttleReasonLabel(plan)}
            </span>
            <span
              className="infperf-sub"
              title="the Local sub-tier the plan prefers under pressure"
            >
              PREFER {throttleTierPrefLabel(plan)}
            </span>
            {plan.deferHeavy && (
              <span
                className="infperf-sub defer"
                title="the plan defers heavy work (e.g. speculative's extra draft pass) this turn"
              >
                DEFER HEAVY
              </span>
            )}
          </>
        )}
      </div>
      <div className="infperf-note dim-note">{throttleHonest(plan)}</div>
    </div>
  );
}

/** QUANTIZATION — the quant that ACTUALLY loaded (not merely the requested one),
 *  with the honest fallback note. */
function QuantRow({ perf }: { perf: InferencePerfStatus }) {
  const known = quantIsKnown(perf);
  return (
    <div className="infperf-row">
      <div className="infperf-row-head">
        <span className="infperf-title">QUANTIZATION</span>
        <span className={`dot ${known ? "ok" : "off"}`} aria-hidden="true" />
        <span
          className={`infperf-pill quant-${known ? "known" : "unknown"}`}
          title="the quant that ACTUALLY loaded this turn — not merely the one requested (the server falls back + reports the real one)"
        >
          {quantLabel(perf)}
        </span>
      </div>
      <div className="infperf-note dim-note">{quantHonest(perf)}</div>
    </div>
  );
}
