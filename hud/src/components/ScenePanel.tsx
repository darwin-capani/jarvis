import type { SceneStatus } from "../core/events";
import Frame from "./Frame";

/**
 * SCENE // ACOUSTIC AWARENESS — the honest state of the ambient sound-event
 * sensor (daemon scene.rs, F6).
 *
 * HONESTY CONTRACT (do not regress):
 *   - SHIPS OFF: OFF unless the operator enables [scene]. The pill says so.
 *   - INERT UNTIL FULLY WIRED: ARMED · NEEDS MODEL until a classifier model is
 *     bundled, then ARMED · NO CAPTURE until the mic tap is wired; only with all
 *     three does it read LISTENING. A present model alone is never "listening".
 *   - NEVER RETAINS AUDIO: the footnote states, and `retainsAudio` pins, that
 *     only event labels leave the classifier — never a waveform.
 */
export default function ScenePanel({ scene }: { scene: SceneStatus | null }) {
  if (scene === null) return null;

  const state = sensorState(scene);
  return (
    <div className="scene-panel">
      <Frame title="SCENE // ACOUSTIC AWARENESS" tag="NEVER RETAINS AUDIO">
        <div className="scene-body">
          <div className="scene-head">
            <span className={`scene-pill ${state.cls}`}>{state.label}</span>
            <span className="scene-count dim-note">
              {scene.vocabulary.length} event type{scene.vocabulary.length === 1 ? "" : "s"} known
            </span>
          </div>
          {scene.recentEvents.length > 0 ? (
            <ul className="scene-events">
              {scene.recentEvents.map((e, i) => (
                <li key={`${e.label}-${e.ts}-${i}`} className="scene-event">
                  <span className="scene-event-label">{e.label.replace(/_/g, " ")}</span>
                  <span className="scene-event-conf dim-note">{Math.round(e.confidence * 100)}%</span>
                </li>
              ))}
            </ul>
          ) : (
            scene.vocabulary.length > 0 && (
              <div className="scene-vocab dim-note">
                Listens for: {scene.vocabulary.slice(0, 8).map((v) => v.replace(/_/g, " ")).join(", ")}
                {scene.vocabulary.length > 8 ? "…" : ""}
              </div>
            )
          )}
          <div className="scene-foot dim-note">
            Ambient sound events only — never speech, never a recording. Only event
            labels leave the classifier; audio is never retained. Inert until a
            classifier model is bundled.
          </div>
        </div>
      </Frame>
    </div>
  );
}

function sensorState(s: SceneStatus): { label: string; cls: string } {
  if (!s.enabled) return { label: "OFF", cls: "off" };
  if (!s.classifierPresent) return { label: "ARMED · NEEDS MODEL", cls: "armed" };
  if (!s.captureWired) return { label: "ARMED · NO CAPTURE", cls: "armed" };
  return { label: "LISTENING", cls: "ready" };
}
