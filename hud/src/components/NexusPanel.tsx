import type { CSSProperties } from "react";
import {
  NEXUS_APP,
  NEXUS_SPECTRUM_BANDS,
  NEXUS_TOPIC_CLIPPING,
  NEXUS_TOPIC_GAIN,
  NEXUS_TOPIC_LEVELS,
  NEXUS_TOPIC_ROUTES,
  NEXUS_TOPIC_SPECTRUM,
  parseNexusClipping,
  parseNexusGain,
  parseNexusLevels,
  parseNexusRoutes,
  parseNexusSpectrum,
  type NexusChannelLevel,
} from "../core/events";
import type { AppFeed } from "../core/state";
import { agentProfile } from "../core/agents";
import Frame from "./Frame";

/** Manifest name of the nexus micro-app (apps/nexus/manifest.toml). The panel
 *  renders exactly this app's feed slice; other app names are ignored here
 *  (each surface owns its own component). Nexus is runtime="python" (control
 *  plane) hosting a Rust cdylib realtime/DSP core via ctypes; audio=true,
 *  net_hosts=[] offline. The CoreAudio IOProc, aggregate device, the sub-10ms
 *  monitor RTT and AUv3 hosting run ON-DEVICE against real audio hardware and
 *  CANNOT run headlessly — so every LIVE number here (meters, LUFS, spectrum,
 *  measured_rtt_ms, clip events) appears ONLY when the on-device audio core is
 *  running. This panel is the HUD-side telemetry READOUT, never a synthesizer:
 *  it ships no audio and fabricates no measurement. */
export const NEXUS_APP_NAME = NEXUS_APP;

/** Nexus carries the MUSIC/AUDIO agent's hue. Jerome (Leisure + DJ) is the
 *  studio/music persona in the static roster (config/agents.toml mirror —
 *  jerome = 340, a warm magenta). Drives the panel accent so the surface reads
 *  as the audio agent's, distinct from the cyan default and Vision's violet.
 *  RED (--alert-red) stays reserved for the clip flash / alerts only. */
const NEXUS_HUE = agentProfile("jerome")?.hue ?? 340;

/** dBFS display floor — meters and the spectrum strip map this..0 dBFS onto the
 *  0..100% fill. Anything at or below reads as silence (empty). */
const DBFS_FLOOR = -60;

/** The true-peak clip ceiling (SPEC §3: clip detect at -1 dBFS, 4x oversampled).
 *  A `measured_rtt_ms` / meter readout is honest telemetry; this is just the
 *  display threshold for the recent-clip affordance. */
const CLIP_CEILING_DBFS = -1;

/** Format a dBFS value compactly: an -inf-ish floor reads "−∞", else 1 dp with a
 *  unicode minus so columns align. */
function dbfs(v: number): string {
  if (!Number.isFinite(v) || v <= DBFS_FLOOR) return "−∞";
  const r = v.toFixed(1);
  return r.startsWith("-") ? `−${r.slice(1)}` : r;
}

/** Format a LUFS readout (or "—" when the on-device core has not reported it —
 *  never a fabricated loudness). */
function lufs(v: number | null): string {
  if (v === null || !Number.isFinite(v)) return "—";
  const r = v.toFixed(1);
  return r.startsWith("-") ? `−${r.slice(1)}` : r;
}

/** Map a dBFS level onto a 0..1 fill against the DBFS_FLOOR..0 window. */
function fill(dbfsVal: number): number {
  if (!Number.isFinite(dbfsVal)) return 0;
  const clamped = Math.max(DBFS_FLOOR, Math.min(0, dbfsVal));
  return (clamped - DBFS_FLOOR) / (0 - DBFS_FLOOR);
}

/** One vertical meter (peak cap + rms body) for a single channel. */
function Meter({ level, label }: { level: NexusChannelLevel; label: string }) {
  const rmsPct = fill(level.rmsDbfs) * 100;
  const peakPct = fill(level.peakDbfs) * 100;
  const hot = level.peakDbfs > CLIP_CEILING_DBFS;
  return (
    <div className="nx-meter">
      <div className="nx-meter-track">
        <span className="nx-meter-rms" style={{ height: `${rmsPct}%` }} />
        <span
          className={`nx-meter-peak${hot ? " hot" : ""}`}
          style={{ bottom: `${peakPct}%` }}
          aria-hidden="true"
        />
      </div>
      <span className="nx-meter-val">{dbfs(level.peakDbfs)}</span>
      <span className="nx-meter-label">{label}</span>
    </div>
  );
}

export default function NexusPanel({
  feed,
  running,
}: {
  /** The nexus app's feed slice, or undefined if it never reported. */
  feed: AppFeed | undefined;
  /** Tracked-running flag from state.runningApps (authoritative over feed). */
  running: boolean;
}) {
  // Live (running) OR a feed that has reported => online. A stopped app that
  // previously reported keeps showing its last telemetry, dimmed.
  const online = running || feed?.running === true;
  const topics = feed?.topics ?? {};

  const levels = topics[NEXUS_TOPIC_LEVELS]
    ? parseNexusLevels(topics[NEXUS_TOPIC_LEVELS])
    : null;
  const routes = topics[NEXUS_TOPIC_ROUTES]
    ? parseNexusRoutes(topics[NEXUS_TOPIC_ROUTES])
    : null;
  const gain = topics[NEXUS_TOPIC_GAIN]
    ? parseNexusGain(topics[NEXUS_TOPIC_GAIN])
    : null;
  const clipping = topics[NEXUS_TOPIC_CLIPPING]
    ? parseNexusClipping(topics[NEXUS_TOPIC_CLIPPING])
    : null;
  const spectrum = topics[NEXUS_TOPIC_SPECTRUM]
    ? parseNexusSpectrum(topics[NEXUS_TOPIC_SPECTRUM])
    : null;

  const hasData = !!(levels || routes || gain || clipping || spectrum);

  // A sparse-matrix lookup: which (in,out) crosspoints are live routes.
  const routeSet = new Set(routes?.matrix.map((c) => `${c.in}:${c.out}`));
  const gainAt = new Map(routes?.matrix.map((c) => [`${c.in}:${c.out}`, c.gainDb]));

  // Header tag: a recent clip wins (alert), then the measured monitor RTT when
  // the on-device core has measured it, then the meter channel count, else
  // OFFLINE. measured_rtt_ms is ONLY ever shown when the device reported it.
  const tag = !online
    ? "OFFLINE"
    : clipping
      ? "CLIP"
      : routes && routes.measuredRttMs !== null
        ? `${routes.measuredRttMs.toFixed(1)}ms RTT`
        : levels
          ? `${levels.ch.length} CH`
          : "ON DEVICE";

  // The whole panel carries the Nexus (audio agent) hue as a CSS custom
  // property so the accents read as the studio surface's magenta. RED stays
  // reserved for the clip flash only.
  const style = { ["--nexus-hue" as string]: String(NEXUS_HUE) } as CSSProperties;

  return (
    <Frame
      className={`nexus ${online ? "" : "offline"}${clipping ? " clip" : ""}`}
      title="NEXUS // AUDIO ROUTING MATRIX"
      tag={tag}
    >
      {!online && !hasData ? (
        <div className="nx-placeholder" style={style}>
          <div className="nx-ph-big">NEXUS OFFLINE</div>
          <div className="nx-ph-small">say "open nexus"</div>
        </div>
      ) : (
        <div className="nx-body" style={style}>
          {/* On-device honesty banner. The CoreAudio IOProc, the aggregate
              device and the loopback RTT measurement run on the device against
              real hardware; this panel is the telemetry readout, never a
              synthesizer, and ships no audio. */}
          <div className="nx-surface">
            <span className="nx-surface-dot" aria-hidden="true" />
            <span className="nx-surface-text">REALTIME CORE RUNS ON DEVICE</span>
            <span className="nx-surface-sub">CoreAudio IOProc · offline · HUD reads telemetry</span>
          </div>

          {/* audio.clipping — true-peak clip event (SPEC §3, -1 dBFS, 4x
              oversampled). The ONLY place this panel uses --alert-red; the
              .clip class on the Frame also pulses a flash. */}
          {clipping ? (
            <div className="nx-clip">
              <span className="nx-clip-badge" aria-hidden="true" />
              <span className="nx-clip-text">CLIP</span>
              <span className="nx-clip-ch">CH {clipping.channel}</span>
              <span className="nx-clip-tp">{dbfs(clipping.truePeakDbfs)} dBTP</span>
            </div>
          ) : null}

          {/* audio.routes — the N-input × M-output crosspoint grid (SPEC §1).
              Live crosspoints (above -inf) light; the on-device matrix state is
              truth, this mirrors its snapshot. measured_rtt_ms reads honestly:
              a value ONLY when the loopback measurement ran on-device. */}
          {routes ? (
            <div className="nx-routes">
              <div className="nx-routes-head">
                <span className="nx-routes-label">MATRIX</span>
                <span className="nx-routes-dim">
                  {routes.inputs}×{routes.outputs}
                </span>
                <span className="nx-routes-rtt">
                  RTT{" "}
                  {routes.measuredRttMs !== null
                    ? `${routes.measuredRttMs.toFixed(1)}ms`
                    : "—"}
                </span>
              </div>
              {routes.inputs > 0 && routes.outputs > 0 ? (
                <div
                  className="nx-grid"
                  style={{
                    gridTemplateColumns: `var(--nx-rowhdr) repeat(${routes.outputs}, 1fr)`,
                  }}
                >
                  {/* corner + output column headers */}
                  <span className="nx-grid-corner" aria-hidden="true" />
                  {Array.from({ length: routes.outputs }, (_, o) => (
                    <span key={`oh-${o}`} className="nx-grid-colhdr">
                      O{o}
                    </span>
                  ))}
                  {/* one row per input: header + crosspoint cells */}
                  {Array.from({ length: routes.inputs }, (_, i) => (
                    <RowCells
                      key={`r-${i}`}
                      i={i}
                      outputs={routes.outputs}
                      routeSet={routeSet}
                      gainAt={gainAt}
                    />
                  ))}
                </div>
              ) : (
                <div className="nx-grid-empty dim-note">matrix not configured</div>
              )}
            </div>
          ) : null}

          {/* audio.levels — vertical meter pair + the three BS.1770-4 LUFS
              readouts (SPEC §6). All live on-device; LUFS reads "—" until the
              core reports it (never a fabricated loudness). */}
          {levels ? (
            <div className="nx-levels">
              <div className="nx-meters">
                {levels.ch.length > 0 ? (
                  levels.ch.map((c, i) => (
                    <Meter key={`m-${i}`} level={c} label={`${i}`} />
                  ))
                ) : (
                  <div className="nx-meters-empty dim-note">no input channels</div>
                )}
              </div>
              <div className="nx-lufs">
                <div className="nx-lufs-stat">
                  <span className="nx-lufs-label">LUFS-M</span>
                  <span className="nx-lufs-val">{lufs(levels.lufsM)}</span>
                </div>
                <div className="nx-lufs-stat">
                  <span className="nx-lufs-label">LUFS-S</span>
                  <span className="nx-lufs-val">{lufs(levels.lufsS)}</span>
                </div>
                <div className="nx-lufs-stat">
                  <span className="nx-lufs-label">LUFS-I</span>
                  <span className="nx-lufs-val">{lufs(levels.lufsI)}</span>
                </div>
              </div>
            </div>
          ) : null}

          {/* audio.spectrum — the 96-band log spectrum strip (SPEC §6). Each bar
              maps a dBFS band onto the DBFS_FLOOR..0 window. */}
          {spectrum ? (
            <div className="nx-spectrum" aria-hidden="true">
              {spectrum.bands.map((b, i) => (
                <span
                  key={`b-${i}`}
                  className="nx-band"
                  style={{ height: `${fill(b) * 100}%` }}
                />
              ))}
            </div>
          ) : null}
          {spectrum ? (
            <div className="nx-spectrum-label">SPECTRUM · {NEXUS_SPECTRUM_BANDS} BANDS</div>
          ) : null}

          {/* audio.gain — last input/output trim change OR mute/unmute (SPEC §6,
              on change). A mute rides the distinct {muted} payload — shown as
              MUTED/UNMUTED, never rendered as a fabricated 0.0 dB. */}
          {gain ? (
            <div className="nx-gain">
              <span className="nx-gain-label">GAIN</span>
              <span className="nx-gain-ch">CH {gain.channel}</span>
              {gain.stage ? <span className="nx-gain-stage">{gain.stage.toUpperCase()}</span> : null}
              {gain.muted !== null ? (
                <span className={`nx-gain-db${gain.muted ? " nx-muted" : ""}`}>
                  {gain.muted ? "MUTED" : "UNMUTED"}
                </span>
              ) : gain.gainDb !== null ? (
                <span className="nx-gain-db">
                  {gain.gainDb >= 0 ? "+" : "−"}
                  {Math.abs(gain.gainDb).toFixed(1)} dB
                </span>
              ) : null}
            </div>
          ) : null}
        </div>
      )}
    </Frame>
  );
}

/** One matrix row: an input header label + a cell per output. A lit cell is a
 *  live crosspoint (route above -inf); its gain is shown for context. */
function RowCells({
  i,
  outputs,
  routeSet,
  gainAt,
}: {
  i: number;
  outputs: number;
  routeSet: Set<string>;
  gainAt: Map<string, number>;
}) {
  return (
    <>
      <span className="nx-grid-rowhdr">I{i}</span>
      {Array.from({ length: outputs }, (_, o) => {
        const key = `${i}:${o}`;
        const on = routeSet.has(key);
        const g = gainAt.get(key);
        return (
          <span
            key={`c-${i}-${o}`}
            className={`nx-cell${on ? " on" : ""}`}
            title={on && g !== undefined ? `I${i}→O${o} ${g.toFixed(1)} dB` : `I${i}→O${o}`}
          >
            {on ? <span className="nx-cell-dot" aria-hidden="true" /> : null}
          </span>
        );
      })}
    </>
  );
}
