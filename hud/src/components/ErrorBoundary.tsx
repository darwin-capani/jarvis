import { Component, type ErrorInfo, type ReactNode } from "react";

interface Props {
  /** Short label for the localized fallback (e.g. a column or panel name). */
  label?: string;
  /** When set, a caught error renders this instead of the default retry chip. */
  fallback?: ReactNode;
  /**
   * When any value in this array CHANGES (shallow compare) while the boundary is
   * errored, the error is cleared and the children are retried. Use a value that
   * changes when the error condition may have resolved (e.g. the connection state
   * on reconnect) — NOT one that changes every render, which would defeat the
   * boundary for a persistent error.
   */
  resetKeys?: readonly unknown[];
  children: ReactNode;
}

interface State {
  error: Error | null;
}

function keysChanged(a: readonly unknown[] | undefined, b: readonly unknown[] | undefined): boolean {
  if (a === b) return false;
  if (!a || !b || a.length !== b.length) return true;
  return a.some((v, i) => !Object.is(v, b[i]));
}

/**
 * FAULT ISOLATION. React 18 unmounts the ENTIRE tree on any uncaught error thrown
 * during render — so without a boundary, one panel doing `x.toFixed()` on an
 * undefined value (a malformed/edge telemetry frame reaching an un-audited panel
 * branch) blanks the whole HUD to nothing: no panels, no StatusBar, no LINK OFFLINE
 * overlay, no in-app recovery — the user must quit and relaunch the desktop app.
 *
 * Wrapping each independently-mounted region (the columns, the overlays, and
 * `<App/>` itself as a top-level backstop) contains a throw to that region: it
 * shows a small "unavailable" fallback while the rest of the HUD — critically the
 * connection overlay and StatusBar — keeps rendering and updating.
 *
 * RECOVERY: HUD render throws are usually TRANSIENT (one bad frame, then good
 * ones). The default fallback offers a Retry action, and `resetKeys` auto-clears
 * the error when a caller-chosen signal changes (e.g. reconnect) — so a panel
 * self-heals rather than staying dead until a full reload.
 *
 * Error boundaries MUST be class components (no hooks equivalent).
 */
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    // Console only — never swallow silently, but never crash the boundary either.
    console.error(`[HUD] ${this.props.label ?? "region"} render error:`, error, info.componentStack);
  }

  componentDidUpdate(prev: Props): void {
    // Auto-retry when a caller-chosen reset signal changes while errored.
    if (this.state.error && keysChanged(prev.resetKeys, this.props.resetKeys)) {
      this.reset();
    }
  }

  private reset = (): void => {
    this.setState({ error: null });
  };

  render(): ReactNode {
    if (this.state.error) {
      if (this.props.fallback !== undefined) return this.props.fallback;
      return (
        <div className="panel-error" role="alert">
          <span className="panel-error-label">{this.props.label ?? "panel"} unavailable</span>
          <button type="button" className="panel-error-retry" onClick={this.reset}>
            retry
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
