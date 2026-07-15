import { useEffect, useRef } from "react";
import type { CaptionEntry } from "../core/state";
import { CAPTION_UNKNOWN_SPEAKER } from "../core/events";
import Frame from "./Frame";

/**
 * HERALD-EARS — LIVE CAPTIONS band (daemon/src/captions.rs -> `captions.line`).
 *
 * Renders the live caption stream the daemon assembles from the on-device STT
 * transcript feed: one row per diarized turn, each carrying the speaker label +
 * the transcript text + (optionally) an on-device translation.
 *
 * HONESTY CONTRACT (do not regress):
 *   - SPEAKER LABELS ARE VERBATIM. The label is exactly what diarize.rs reported —
 *     a distinct "speaker_N" only when the EL-Scribe backend actually diarized,
 *     else the honest single "unknown" stream (on-device whisper has no diarization
 *     model). This band NEVER invents a distinct speaker; an "unknown" row is shown
 *     honestly as UNKNOWN, not dressed up as a named speaker.
 *   - TRANSLATION IS BEST-EFFORT + OPTIONAL. A translation renders ONLY when the
 *     daemon produced a real one (translation !== null); a passthrough / an honest
 *     offline degrade rides translation:null and the row shows just the heard text —
 *     never a fabricated rendering. The translation is labelled as an on-device,
 *     best-effort rendering (quality is bounded by the local model).
 *   - READ-ONLY DISPLAY. No button, no action, no network — a caption is pure
 *     presentation of what was heard.
 *   - SHIPS OFF. The [captions].enabled gate ships false, so until it is enabled the
 *     daemon emits no captions.line and this band renders NOTHING (mirrors the other
 *     event-fed panels).
 */
export default function CaptionBand({ captions }: { captions: CaptionEntry[] }) {
  const scroller = useRef<HTMLDivElement>(null);
  // Depend on the newest row's IDENTITY, not the array length — once the ring hits
  // CAPTIONS_CAP the length is constant forever and a length-keyed effect would never
  // re-fire (the same autoscroll-at-the-cap bug the transcript panel fixed).
  const lastSeq = captions.length > 0 ? captions[captions.length - 1].seq : 0;

  useEffect(() => {
    const el = scroller.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [lastSeq]);

  // Nothing to show until a captions.line arrives. The [captions].enabled gate ships
  // OFF, so the reducer holds `captions` empty until it is enabled AND the transcript
  // path emits a row — render nothing rather than a placeholder.
  if (captions.length === 0) return null;

  return (
    <Frame className="captions" title="COMMS // LIVE CAPTIONS" tag={`${captions.length} LN`}>
      <div className="captions-scroll" ref={scroller}>
        {captions.map((c) => {
          const unknown = c.speaker === CAPTION_UNKNOWN_SPEAKER;
          return (
            <div key={c.seq} className={`caption-line ${unknown ? "unknown" : "speaker"}`}>
              <span
                className="caption-speaker"
                title={
                  unknown
                    ? "unseparated audio — a single honest stream (no diarization model), never a fabricated speaker"
                    : `speaker label from diarization: ${c.speaker}`
                }
              >
                {unknown ? "UNKNOWN" : c.speaker.toUpperCase()}
              </span>
              <span className="caption-text">{c.text}</span>
              {c.translation !== null && (
                <span className="caption-translation" title="on-device translation (best-effort; quality bounded by the local model)">
                  {c.translation}
                </span>
              )}
            </div>
          );
        })}
      </div>
      <div className="captions-foot dim-note">
        Live captions of what was heard. Speaker labels are exactly what diarization
        reported — a single <code>UNKNOWN</code> stream when speakers can&rsquo;t be
        separated, never a fabricated speaker. A translation shows only when one was
        actually produced on-device (best-effort); offline it is simply omitted, never
        faked. Read-only display; ships OFF behind <code>[captions].enabled</code>.
      </div>
    </Frame>
  );
}
