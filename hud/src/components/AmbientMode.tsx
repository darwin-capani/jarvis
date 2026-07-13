import type { Presence } from "../core/events";

/**
 * The at-rest ambient "mirror": shown fullscreen over the HUD when you step away
 * (see core/ambient.ts). Calm and glanceable — a large clock/date, the fused
 * presence, and a couple of at-a-glance counts — instead of the dense telemetry
 * grid. Pure: `now` and the counts are passed in, so it renders deterministically
 * and is unit-testable (no internal timer, no direct state access).
 */
export default function AmbientMode({
  now,
  presence,
  briefCount,
  feedCount,
}: {
  now: Date;
  presence: Presence | null;
  briefCount: number;
  feedCount: number;
}) {
  return (
    <div className="ambient-mode" role="status" aria-label="JARVIS at rest">
      <div className="ambient-clock">{formatTime(now)}</div>
      <div className="ambient-date">{formatDate(now)}</div>
      <div className="ambient-line dim-note">
        <span className={`ambient-presence ${presence?.state ?? "unknown"}`}>
          {presenceLabel(presence)}
        </span>
        {briefCount > 0 && <span className="ambient-stat">{briefCount} in your brief</span>}
        {feedCount > 0 && <span className="ambient-stat">{feedCount} live feeds</span>}
      </div>
      <div className="ambient-foot dim-note">Move the mouse or speak to wake the console.</div>
    </div>
  );
}

function pad(n: number): string {
  return String(n).padStart(2, "0");
}

/** 24-hour HH:MM — deterministic from the passed Date (no locale surprises in
 *  tests). */
function formatTime(d: Date): string {
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

const DAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS = [
  "January", "February", "March", "April", "May", "June",
  "July", "August", "September", "October", "November", "December",
];

function formatDate(d: Date): string {
  return `${DAYS[d.getDay()]}, ${MONTHS[d.getMonth()]} ${d.getDate()}`;
}

function presenceLabel(p: Presence | null): string {
  switch (p?.state) {
    case "away":
      return "Away";
    case "focused":
      return "In flow";
    case "present":
      return "Present";
    default:
      return "Standing by";
  }
}
