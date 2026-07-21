import { useEffect, useRef } from "react";
import type { IntentChip, TranscriptLine } from "../core/state";
import Frame from "./Frame";

export default function TranscriptPanel({
  lines,
  intent,
}: {
  lines: TranscriptLine[];
  intent: IntentChip | null;
}) {
  const scroller = useRef<HTMLDivElement>(null);
  // Depend on the newest line's IDENTITY, not the array length — once the
  // ring buffer hits TRANSCRIPT_CAP the length is constant forever and a
  // length-keyed effect would never re-fire (autoscroll died at the cap).
  const lastSeq = lines.length > 0 ? lines[lines.length - 1].seq : 0;

  useEffect(() => {
    const el = scroller.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [lastSeq]);

  return (
    <Frame
      className="transcript"
      title="COMMS // TRANSCRIPT"
      tag={lines.length > 0 ? `${lines.length} LN` : "—"}
    >
      {/* a11y: role="log" (implicit polite live region announcing ADDITIONS
          only) — lines land COMPLETE via pushTranscript, one per finished
          utterance, so this is exactly the chat-log semantics and never
          re-announces the whole history. */}
      <div
        className="transcript-scroll"
        ref={scroller}
        role="log"
        aria-label="Conversation transcript"
      >
        {lines.length === 0 && (
          <div className="line">
            <span className="text dim-note">awaiting first exchange…</span>
          </div>
        )}
        {lines.map((l) => (
          <div key={l.seq} className={`line ${l.who} ${l.routedTo === "cloud" ? "cloud" : ""}`}>
            <span className="who">
              {l.who === "user" ? "YOU" : l.routedTo === "cloud" ? "DARWIN · CLOUD" : "DARWIN"}
            </span>
            <span className="text">{l.text}</span>
          </div>
        ))}
      </div>
      {intent && (
        <div className={`intent-chip ${intent.confidence < 0.6 ? "low" : ""}`}>
          <span>{intent.intent}</span>
          <span className="conf">
            <i style={{ width: `${Math.round(Math.min(1, Math.max(0, intent.confidence)) * 100)}%` }} />
          </span>
          <span>{intent.confidence.toFixed(2)}</span>
          {intent.complexity && <span>· {intent.complexity}</span>}
        </div>
      )}
    </Frame>
  );
}
