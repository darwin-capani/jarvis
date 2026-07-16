import type { ObolSpend, ObolPressure } from "../core/events";
import Frame from "./Frame";

/**
 * SPEND // CLOUD METER — the MEASURED daily cloud-spend gauge + the REDUCE-ONLY
 * dollar-budget posture.
 *
 * Fed by the daemon's periodic `obol.spend` telemetry (daemon/src/obol.rs, every
 * 30s with a 20s startup delay). AGGREGATE-ONLY + REVIEW-ONLY by construction:
 *
 *   - MEASURED, NEVER FABRICATED — today's spend is summed from real recorded
 *     cloud calls (the SAME token counts the eval cost window uses). Before the
 *     first call the gauge reads $0.00, never an invented number.
 *   - $ IS AN ESTIMATE — every dollar figure is a published $/1M-token rate over
 *     the measured token counts, NOT a billed number. Always labelled EST.
 *   - REDUCE-ONLY BUDGET — when a daily cap is set, the budget can only step a
 *     near/over-cap turn DOWN to a cheaper/on-device tier. It NEVER loosens a
 *     gate, NEVER escalates to a pricier model, and an explicit override beats it.
 *     Under the shipped no-cap default the meter is pure accounting (no step-down).
 *   - SECRET-FREE — the wire (and this component) carry only dollars, token
 *     counts, model/agent names, and the reduce-only posture. Never an utterance.
 *
 * The reducer only ever sets `obolSpend` from a defensively-parsed `obol.spend`
 * (parseObolSpend never returns null and never carries PII), so this component can
 * trust the fields it is handed.
 */
export default function SpendMeter({ spend }: { spend: ObolSpend | null }) {
  // No meter yet (daemon has not emitted obol.spend — the first lands ~20s after
  // startup) — render nothing, mirroring the other event-fed panels.
  if (spend === null) return null;

  const {
    daySpendUsd,
    dailyCapUsd,
    capConfigured,
    headroomUsd,
    fraction,
    pressure,
    callsToday,
    recent,
  } = spend;

  // The gauge fill is the share of the cap spent, clamped to [0, 1] for the bar
  // width (an over-cap fraction > 1 is pinned full; the pressure pill says FLOOR).
  const fillPct = capConfigured ? Math.min(1, Math.max(0, fraction)) * 100 : 0;

  return (
    <div className="spend-meter">
      <Frame title="SPEND // CLOUD METER" tag="MEASURED · REDUCE-ONLY">
        <div className="spend-body">
          <section className="spend-section">
            <div className="spend-section-head">
              <span className="spend-section-title">TODAY</span>
              <span
                className="spend-runtime-pill"
                title="summed from real cloud calls — runtime-gated"
              >
                CLOUD CALLS
              </span>
            </div>

            <div className="spend-metrics">
              <Stat
                label="DAY SPEND"
                value={`~$${fmtUsd(daySpendUsd)}`}
                prominent
                title="ESTIMATE — published $/1M-token rate over the measured token counts, not a billed figure"
              />
              {capConfigured ? (
                <>
                  <Stat label="DAILY CAP" value={`$${fmtUsd(dailyCapUsd)}`} />
                  <Stat label="HEADROOM" value={`$${fmtUsd(headroomUsd)}`} prominent />
                </>
              ) : (
                <Stat
                  label="DAILY CAP"
                  value="NO CAP"
                  title="[obol].daily_usd_cap ships 0.0 — the budget is INERT (pure accounting). Set a cap to arm the reduce-only budget-floor."
                />
              )}
              <Stat label="CALLS TODAY" value={String(callsToday)} />
              <PressurePill pressure={pressure} capConfigured={capConfigured} />
            </div>

            {capConfigured && (
              <div
                className={`spend-gauge ${pressure}`}
                role="img"
                aria-label={`${fmtPctOfCap(fraction)} of the daily cap spent`}
                title={`${fmtPctOfCap(fraction)} of the daily $${fmtUsd(dailyCapUsd)} cap spent today`}
              >
                {/* The ease shoulder marker (~80% of the cap) — where a Heavy turn
                    begins stepping down to Fast. */}
                <span className="spend-gauge-ease" aria-hidden="true" />
                <span className="spend-gauge-fill" style={{ width: `${fillPct}%` }} />
              </div>
            )}
          </section>

          {recent.length > 0 && (
            <section className="spend-section spend-recent">
              <div className="spend-section-head">
                <span className="spend-section-title">RECENT CALLS</span>
              </div>
              <ul className="spend-rows">
                {recent.slice(0, 6).map((r, i) => (
                  <li className="spend-row" key={`${r.ts}-${i}`}>
                    <span className="spend-row-agent">{r.agent || "cloud"}</span>
                    <span className="spend-row-model">{r.model || "—"}</span>
                    <span className="spend-row-tokens dim-note">
                      {fmtTokens(r.inputTokens + r.outputTokens + r.cacheReadTokens)} tok
                    </span>
                    <span className="spend-row-cost">~${fmtUsd(r.costUsd)}</span>
                  </li>
                ))}
              </ul>
            </section>
          )}

          <div className="spend-foot dim-note">
            $ figures are an ESTIMATE (published $/1M-token rate × measured token
            counts), never a billed number. The budget is REDUCE-ONLY: it can only
            route a near/over-cap turn to a cheaper/on-device tier — it never
            loosens a gate, never escalates, and an explicit override beats it.
          </div>
        </div>
      </Frame>
    </div>
  );
}

/** The REDUCE-ONLY budget-pressure pill: NONE (below the cap / no cap), EASE
 *  (~80% — Heavy steps down to Fast), FLOOR (at/over the cap — forced on-device).
 *  Honest: with no cap the pressure is always NONE (pure accounting). */
function PressurePill({
  pressure,
  capConfigured,
}: {
  pressure: ObolPressure;
  capConfigured: boolean;
}) {
  const label = !capConfigured ? "NO CAP" : pressure.toUpperCase();
  const title = !capConfigured
    ? "No daily cap set — the budget is inert (pure accounting), no tier is stepped down."
    : pressure === "floor"
      ? "At/over the daily cap — turns are floored to the on-device Local path (no further cloud spend). An explicit override still beats this."
      : pressure === "ease"
        ? "~80% of the daily cap spent — a Heavy turn eases down to Fast to stay under it."
        : "Below the daily cap — no step-down; routing is unchanged.";
  return (
    <div className={`spend-pressure-pill ${capConfigured ? pressure : "none"}`} title={title}>
      <span className="spend-pressure-label">BUDGET</span>
      <span className="spend-pressure-value">{label}</span>
    </div>
  );
}

/** One measured stat cell (mirrors EvalPanel's Stat). */
function Stat({
  label,
  value,
  prominent,
  title,
}: {
  label: string;
  value: string;
  prominent?: boolean;
  title?: string;
}) {
  return (
    <div className={`spend-stat ${prominent ? "prominent" : ""}`} title={title}>
      <span className="spend-stat-label">{label}</span>
      <span className="spend-stat-value">{value}</span>
    </div>
  );
}

/* formatting helpers ------------------------------------------------------- */

/** A dollar figure to a sensible precision: 4 dp under a cent, else 2 dp. Never
 *  NaN/negative (the parser already floors those, but be defensive). */
function fmtUsd(v: number): string {
  const x = Number.isFinite(v) && v >= 0 ? v : 0;
  return x > 0 && x < 0.01 ? x.toFixed(4) : x.toFixed(2);
}

/** Share of the cap spent, as a whole-ish percent (e.g. 0.83 -> "83%"). */
function fmtPctOfCap(fraction: number): string {
  const x = Number.isFinite(fraction) && fraction >= 0 ? fraction : 0;
  return `${Math.round(x * 100)}%`;
}

/** A token COUNT with a k/M suffix for readability (e.g. 210000 -> "210k"). */
function fmtTokens(n: number): string {
  const x = Number.isFinite(n) && n >= 0 ? Math.floor(n) : 0;
  if (x >= 1_000_000) return `${(x / 1_000_000).toFixed(1)}M`;
  if (x >= 1_000) return `${Math.round(x / 1_000)}k`;
  return String(x);
}
