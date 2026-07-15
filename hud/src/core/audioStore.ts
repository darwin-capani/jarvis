/**
 * Mutable audio-level store — the 15Hz `audio.level` fast path.
 *
 * Live rms frames arrive ~every 66ms. Routing them through React state
 * re-rendered the entire tree per frame (the verified re-render storm), so
 * the WS handler writes them HERE instead, and the only two consumers that
 * care — the Waveform canvas rAF loop and the CoreScene useFrame loop —
 * read the current values directly each frame. No React involvement.
 *
 * The pure reducer in state.ts still receives audio.level envelopes for the
 * listening/idle state machine, but returns reference-identical state when
 * nothing state-machine-visible changed.
 *
 * Pure TypeScript (no DOM/React imports) — unit-tested in
 * src/test/audioStore.test.ts.
 */

export const RMS_HISTORY = 128;

export interface AudioStore {
  /** Append one frame. O(1) ring write — no allocation. */
  push(rms: number, speaking: boolean): void;
  /** Sample i of RMS_HISTORY, oldest first. */
  at(i: number): number;
  /** Mean of the whole history window (drives the waveform idle shimmer). */
  mean(): number;
  /** Most recent rms frame. */
  readonly lastRms: number;
  /** Daemon-side is_speaking(): mic muted because DARWIN is talking. */
  readonly micMuted: boolean;
  /** Monotonic write counter (cheap change detection for consumers). */
  readonly version: number;
  /** Zero everything (used on telemetry disconnect and in tests). */
  reset(): void;
}

export function createAudioStore(): AudioStore {
  const ring = new Float32Array(RMS_HISTORY);
  let head = 0; // index of the OLDEST sample
  let sum = 0;
  let lastRms = 0;
  let micMuted = false;
  let version = 0;

  return {
    push(rms: number, speaking: boolean): void {
      if (!Number.isFinite(rms) || rms < 0) return;
      sum += rms - ring[head];
      ring[head] = rms;
      head = (head + 1) % RMS_HISTORY;
      lastRms = rms;
      micMuted = speaking;
      version += 1;
    },
    at(i: number): number {
      return ring[(head + i) % RMS_HISTORY];
    },
    mean(): number {
      return sum / RMS_HISTORY;
    },
    get lastRms() {
      return lastRms;
    },
    get micMuted() {
      return micMuted;
    },
    get version() {
      return version;
    },
    reset(): void {
      ring.fill(0);
      head = 0;
      sum = 0;
      lastRms = 0;
      micMuted = false;
      version += 1;
    },
  };
}

/** The app-wide singleton: written by the WS handler in App.tsx, read by
 *  Waveform.tsx and CoreScene.tsx inside their own frame loops. */
export const audioStore = createAudioStore();
