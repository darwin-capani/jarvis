/**
 * Telemetry WebSocket client. The HUD is a pure CLIENT of the daemon's
 * broadcast server on ws://127.0.0.1:7177 — it never binds that port and
 * never sends anything meaningful (darwind ignores inbound frames).
 *
 * Reconnect: 1s -> 5s linear backoff, forever. The backoff counter resets
 * only after a connection has stayed open for HEALTHY_RESET_MS — a
 * connect-flap (handshake succeeds, daemon dies immediately, e.g. a crash
 * loop or a heal-applied restart) keeps escalating instead of strobing the
 * offline overlay at 1Hz. Parsing lives in core/events.ts so it stays
 * headlessly testable.
 */

export const TELEMETRY_URL = "ws://127.0.0.1:7177";

export interface TelemetryLinkHandlers {
  onOpen(): void;
  onClose(): void;
  onRaw(raw: string): void;
}

export const BACKOFF_BASE_MS = 1000;
export const BACKOFF_CAP_MS = 5000;
/** A connection must stay open this long before backoff resets to 1s. */
export const HEALTHY_RESET_MS = 3000;

/** Pure backoff schedule: attempts already made -> next delay. Tested. */
export function backoffDelayMs(attempts: number): number {
  return Math.min(BACKOFF_BASE_MS * (attempts + 1), BACKOFF_CAP_MS);
}

/** Pure flap detector: did the connection live long enough to call the
 *  link healthy (and reset the backoff)? Tested. */
export function connectionWasHealthy(openedAtMs: number, closedAtMs: number): boolean {
  return closedAtMs - openedAtMs >= HEALTHY_RESET_MS;
}

export class TelemetryLink {
  private ws: WebSocket | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private attempts = 0;
  private openedAt: number | null = null;
  private stopped = false;

  constructor(
    private readonly url: string,
    private readonly handlers: TelemetryLinkHandlers,
  ) {}

  start(): void {
    this.stopped = false;
    this.connect();
  }

  stop(): void {
    this.stopped = true;
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    if (this.ws) {
      // Detach handlers first so the close does not schedule a reconnect.
      this.ws.onopen = null;
      this.ws.onclose = null;
      this.ws.onerror = null;
      this.ws.onmessage = null;
      this.ws.close();
      this.ws = null;
    }
  }

  private connect(): void {
    if (this.stopped) return;
    let ws: WebSocket;
    try {
      ws = new WebSocket(this.url);
    } catch {
      this.scheduleReconnect();
      return;
    }
    this.ws = ws;

    ws.onopen = () => {
      // Do NOT reset attempts here — only a connection that survives
      // HEALTHY_RESET_MS earns the reset (see onclose).
      this.openedAt = Date.now();
      this.handlers.onOpen();
    };
    ws.onmessage = (ev: MessageEvent) => {
      if (typeof ev.data === "string") this.handlers.onRaw(ev.data);
    };
    ws.onerror = () => {
      // onclose always follows; nothing to do here.
    };
    ws.onclose = () => {
      if (this.openedAt !== null && connectionWasHealthy(this.openedAt, Date.now())) {
        this.attempts = 0;
      }
      this.openedAt = null;
      this.ws = null;
      this.handlers.onClose();
      this.scheduleReconnect();
    };
  }

  private scheduleReconnect(): void {
    if (this.stopped || this.timer !== null) return;
    const delay = backoffDelayMs(this.attempts);
    this.attempts += 1;
    this.timer = setTimeout(() => {
      this.timer = null;
      this.connect();
    }, delay);
  }
}
