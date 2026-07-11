import { Component, type ErrorInfo, type ReactNode } from "react";

interface Props {
  /** Short label for the localized fallback (e.g. a column or panel name). */
  label?: string;
  /** When set, a caught error renders this instead of the default chip. */
  fallback?: ReactNode;
  children: ReactNode;
}

interface State {
  error: Error | null;
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

  render(): ReactNode {
    if (this.state.error) {
      if (this.props.fallback !== undefined) return this.props.fallback;
      return (
        <div className="panel-error" role="alert">
          <span className="panel-error-label">{this.props.label ?? "panel"} unavailable</span>
        </div>
      );
    }
    return this.props.children;
  }
}
