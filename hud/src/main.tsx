import ReactDOM from "react-dom/client";
import App from "./App";
import { ErrorBoundary } from "./components/ErrorBoundary";
import "./styles.css";

// Top-level backstop: if the App shell (or a region not otherwise wrapped) throws
// during render, show a recoverable fallback instead of an unrecoverable blank
// #root that forces a desktop-app relaunch.
const AppFallback = (
  <div className="hud-fatal" role="alert">
    <div className="big">HUD ERROR</div>
    <div className="small">The interface hit an unexpected error.</div>
    <button type="button" onClick={() => window.location.reload()}>
      Reload
    </button>
  </div>
);

// No StrictMode: its dev-mode double-mount creates and destroys a second
// WebGL context and a second WebSocket at startup, which reads as a visible
// flash and trips the connection flap detector. The reducer's purity is
// guarded by the vitest suite instead.
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <ErrorBoundary label="HUD" fallback={AppFallback}>
    <App />
  </ErrorBoundary>,
);
