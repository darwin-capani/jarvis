import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import AboutPanel from "./components/AboutPanel";
import ActionPanel from "./components/ActionPanel";
import AgentPanel from "./components/AgentPanel";
import { ErrorBoundary } from "./components/ErrorBoundary";
import AlertPanel from "./components/AlertPanel";
import AudioIoPanel from "./components/AudioIoPanel";
import AnswerSourcesPanel from "./components/AnswerSourcesPanel";
import AuditPanel from "./components/AuditPanel";
import ChartPanel from "./components/ChartPanel";
import CodeIntelPanel from "./components/CodeIntelPanel";
import CommandDeck from "./components/CommandDeck";
import ShellPanel from "./components/ShellPanel";
import UiActuatePanel from "./components/UiActuatePanel";
import DiagnosticsPanel from "./components/DiagnosticsPanel";
import DocSearchPanel from "./components/DocSearchPanel";
import EvalPanel from "./components/EvalPanel";
import FirstRunSetup from "./components/FirstRunSetup";
import ForgePanel from "./components/ForgePanel";
import GlobalScanPanel, { GLOBAL_SCAN_APP } from "./components/GlobalScanPanel";
import ImagePanel from "./components/ImagePanel";
import InferencePerfPanel from "./components/InferencePerfPanel";
import KnowledgeGraphPanel from "./components/KnowledgeGraphPanel";
import LatencyStrip from "./components/LatencyStrip";
import LifeLogPanel from "./components/LifeLogPanel";
import MarkForgePanel, { MARK_FORGE_APP_NAME } from "./components/MarkForgePanel";
import McpPanel from "./components/McpPanel";
import CapabilityAtlasPanel from "./components/CapabilityAtlasPanel";
import PresencePanel from "./components/PresencePanel";
import CapabilityStatusPanel from "./components/CapabilityStatusPanel";
import AmbientMode from "./components/AmbientMode";
import { isAtRest } from "./core/ambient";
import TccSentinelPanel from "./components/TccSentinelPanel";
import IntrospectPanel from "./components/IntrospectPanel";
import AttributionHealthPanel from "./components/AttributionHealthPanel";
import AppDeckPanel from "./components/AppDeckPanel";
import ConnectorMarketplacePanel from "./components/ConnectorMarketplacePanel";
import ExtensibilityPanel from "./components/ExtensibilityPanel";
import MemoryPanel from "./components/MemoryPanel";
import NexusPanel, { NEXUS_APP_NAME } from "./components/NexusPanel";
import OnboardingWizard from "./components/OnboardingWizard";
import ResearchNotebooksPanel from "./components/ResearchNotebooksPanel";
import ReportPanel from "./components/ReportPanel";
import ReticleDial from "./components/ReticleDial";
import SelfHealPanel from "./components/SelfHealPanel";
import SettingsModal from "./components/SettingsModal";
import SiliconCanvasPanel, { SILICON_CANVAS_APP } from "./components/SiliconCanvasPanel";
import SkillsPanel from "./components/SkillsPanel";
import StatusBar from "./components/StatusBar";
import SuggestionsPanel from "./components/SuggestionsPanel";
import UpdateDialog from "./components/UpdateDialog";
import BriefFocusPanel from "./components/BriefFocusPanel";
import BootReveal from "./components/BootReveal";
import ConduitField from "./components/ConduitField";
import CoreHud from "./components/CoreHud";
import UnifiedSearchPanel from "./components/UnifiedSearchPanel";
import VerifyPanel from "./components/VerifyPanel";
import AnswerCrossCheckPanel from "./components/AnswerCrossCheckPanel";
import TakeoverStage from "./components/TakeoverStage";
import Toasts from "./components/Toasts";
import TranscriptPanel from "./components/TranscriptPanel";
import VisionPanel, { VISION_APP } from "./components/VisionPanel";
import Waveform from "./components/Waveform";
import { audioStore } from "./core/audioStore";
import { bool, num, parseEnvelope } from "./core/events";
import { hasSeenOnboarding, markOnboardingSeen } from "./core/onboarding";
import type { OnboardingRouteTarget } from "./core/onboarding";
import { initialState, reduce } from "./core/state";
import {
  initialTakeoverState,
  isExitKey,
  takeoverReduce,
} from "./core/takeover";
import { backendInstalled, bindFullscreenKey, checkForUpdates, inTauri, onAboutMenu, relaunchApp } from "./tauri/bridge";
import { decideShowSetup } from "./core/firstRunSetup";
import {
  decideLaunchUpdateAction,
  isAutoUpdateOn,
  silentUpdateNotice,
} from "./core/autoUpdate";
import { sendCommand } from "./tauri/command";
import { enterTakeover as invokeEnterTakeover, exitTakeover as invokeExitTakeover } from "./tauri/takeover";
import CoreScene from "./three/CoreScene";
import { TELEMETRY_URL, TelemetryLink } from "./ws/client";

export default function App() {
  const [state, dispatch] = useReducer(reduce, undefined, initialState);
  // The Settings modal: null = closed, else the tab to open on. The onboarding
  // wizard routes by opening it on a specific tab (it never adds a new surface).
  const [settingsTab, setSettingsTab] = useState<"credentials" | "system" | null>(null);
  const settingsOpen = settingsTab !== null;
  const [deckOpen, setDeckOpen] = useState(false);

  // CINEMATIC BOOT / WAKE REVEAL — the one-shot power-on overlay. Plays the
  // FIRST time the telemetry link connects (JARVIS "waking"), never on later
  // reconnect flaps (a ref latches it). Purely cosmetic: it gates nothing and
  // self-dismisses on a timer or any key/click.
  const [bootPlaying, setBootPlaying] = useState(false);
  const bootPlayed = useRef(false);
  useEffect(() => {
    if (!bootPlayed.current && state.connected) {
      bootPlayed.current = true;
      setBootPlaying(true);
    }
  }, [state.connected]);
  // REVEALED — flips true when the boot/wake sequence ends, cueing the readout
  // panels to materialize in (the regions "deal in" once, behind/after the boot
  // overlay). Stays false when boot never plays (e.g. dev with no daemon), where
  // the panels are simply visible with no entrance.
  const [revealed, setRevealed] = useState(false);
  // STABLE callback identities: BootReveal arms its timers in a [onReveal,onDone]
  // -keyed effect, so fresh closures each render (telemetry churns App ~15Hz)
  // would clear+re-arm them forever. useCallback pins them. revealPanels fires
  // ~0.6s before the overlay unmounts so the panels deal in UNDER the fade (no
  // empty-HUD gap); dismissBoot then unmounts the overlay.
  const revealPanels = useCallback(() => setRevealed(true), []);
  const dismissBoot = useCallback(() => setBootPlaying(false), []);

  // LAUNCH AUTO-UPDATE — the version string for the "Update available" dialog,
  // or null when no dialog is showing. Set ONLY by the launch auto-check below,
  // and ONLY when the backend reported a REAL available update (status
  // "available") AND the auto-update preference is OFF. It is never set for any
  // other status, so the dialog can never appear on not_configured/up_to_date/
  // error or on a fabricated update.
  const [updateDialogVersion, setUpdateDialogVersion] = useState<string | null>(null);
  // ONCE-PER-LAUNCH guard: a ref (not state) so a re-render never re-fires the
  // launch check, and the StrictMode double-invoke in dev never double-checks.
  const launchUpdateChecked = useRef(false);

  // ABOUT PANEL — the custom "About J.A.R.V.I.S." panel (replaces the macOS
  // standard about panel so it can carry a working "Check for Updates" button +
  // the credit). `aboutOpen` is driven ONLY by the native menu event below;
  // `appVersion` is the version that event carries (the real bundle version).
  const [aboutOpen, setAboutOpen] = useState(false);
  const [appVersion, setAppVersion] = useState("1.0.2");

  // FIRST-RUN SETUP — the honest backend-install gate. `backendIsInstalled` is
  // the RESOLVED backend_installed() result, or null until the shell check
  // returns (and it stays null outside the Tauri shell, where there is nothing to
  // install). The gate (decideShowSetup) shows the setup screen ONLY when, in the
  // shell, the backend is KNOWN-not-installed AND the daemon is not reachable
  // (state.connected === false). A running daemon connects the channel, so a
  // healthy install NEVER sees this. The check is ref-guarded to fire once.
  const [backendIsInstalled, setBackendIsInstalled] = useState<boolean | null>(null);
  const backendCheckStarted = useRef(false);

  // FIRST-RUN ONBOARDING (WS4b item 1): shown ONCE, gated on the REAL persisted
  // flag (localStorage via hasSeenOnboarding). Dismissing/finishing/skipping (or
  // routing away) sets the flag so it never reappears on its own; it can be
  // re-opened from Settings. Initialized lazily from storage so a returning user
  // never even mounts it. Fail-safe: an unavailable storage reads as already-seen,
  // so the wizard never gets stuck re-appearing where it cannot persist.
  const [onboardingOpen, setOnboardingOpen] = useState(() => !hasSeenOnboarding());
  // Dismiss + persist (Skip / Finish / Esc / backdrop / route-away all land here).
  const dismissOnboarding = useCallback(() => {
    markOnboardingSeen();
    setOnboardingOpen(false);
  }, []);
  // A routing step opens the matching EXISTING Settings tab, then dismisses the
  // wizard. It never bypasses the gated action on that surface — it just deep-opens
  // the panel the user completes the step in.
  const onboardingRoute = useCallback(
    (target: Exclude<OnboardingRouteTarget, null>) => {
      setSettingsTab(target === "settings-system" ? "system" : "credentials");
      dismissOnboarding();
    },
    [dismissOnboarding],
  );
  // Re-open the tour from Settings (a deliberate user action — never auto).
  const reopenOnboarding = useCallback(() => {
    setSettingsTab(null);
    setOnboardingOpen(true);
  }, []);

  // KIOSK TAKEOVER — the full-desktop, no-OS-chrome mode. Ships OFF and is
  // NEVER auto-entered: nothing here calls enterTakeover at startup; it is an
  // explicit operator action only (the ENTER TAKEOVER control + enterTakeover()).
  // `takeover.active` mirrors the backend's recorded takeover bit; the actual
  // fullscreen + Dock/menu-bar hide is owned (and fully reversed) by the Tauri
  // backend's enter_takeover/exit_takeover and is device-gated.
  const [takeover, takeoverDispatch] = useReducer(takeoverReduce, undefined, initialTakeoverState);
  const takeoverActive = takeover.active;

  // ENTER: flip the HUD into takeover, then ask the backend to mutate the real
  // window (graceful no-op outside the Tauri shell, so a windowed dev/vitest
  // session can still preview the takeover LAYOUT). The visible EXIT control and
  // the Esc handler below both call exitTakeover.
  const enterTakeover = useCallback(() => {
    takeoverDispatch({ type: "enter" });
    void invokeEnterTakeover();
  }, []);

  // EXIT — the always-available escape hatch. Clears the HUD bit AND asks the
  // backend to reverse every window mutation + restore the Dock/menu bar. Both
  // the in-HUD EXIT control and the Esc key route here. Idempotent + total.
  const exitTakeover = useCallback(() => {
    takeoverDispatch({ type: "exit" });
    void invokeExitTakeover();
  }, []);

  // PANIC / LOCKDOWN (task #12) — the prominent emergency stop. The StatusBar
  // PANIC button fires the DEDICATED `panic` verb (NOT {cmd:"ask"}), so a panic
  // can never leak to the model/answer path; the daemon calls lockdown::panic()
  // directly. The reply carries `locked`, which we fold into the indicator
  // immediately (ahead of the next lockdown.status frame) so the chip flips on the
  // press. Unlock is deliberate + user-only and lives in Settings (LockdownSection
  // sends the `unlock` verb); it routes its reply through the same onLockedChange.
  const onLockedChange = useCallback((locked: boolean) => {
    dispatch({ type: "lockdown.set", locked });
  }, []);
  const panic = useCallback(async () => {
    const r = await sendCommand({ cmd: "panic" });
    if (r.ok && typeof r.locked === "boolean") onLockedChange(r.locked);
  }, [onLockedChange]);

  // ACCEPT a habit-automation offer (#13). This is NOT an ungated create: the
  // panel hands us the offer id + the natural-language standing-mission SETUP
  // request, which we send over the SAME gated command channel a spoken request
  // takes (`ask`). The daemon's selector routes a hard-recurring standing-setup
  // to `standing_create`, which PARKS behind the cross-turn confirmation gate —
  // so accepting still goes through the normal gate and nothing is established
  // until the user confirms there. We then dismiss the offer locally (the same
  // ledger as a manual Dismiss) so an accepted offer is not re-shown.
  const acceptSuggestion = useCallback((id: string, text: string) => {
    void sendCommand({ cmd: "ask", text });
    dispatch({ type: "suggestion.dismiss", id });
  }, []);
  const dismissSuggestion = useCallback(
    (id: string) => dispatch({ type: "suggestion.dismiss", id }),
    [],
  );

  // Dev-only: lets `vite dev` sessions inject actions/envelopes without a
  // daemon (the HUD never binds 7177 itself). Stripped from prod builds.
  if (import.meta.env.DEV) {
    (window as unknown as Record<string, unknown>).__hudDispatch = dispatch;
    (window as unknown as Record<string, unknown>).__audioStore = audioStore;
  }

  // AT-REST / AMBIENT MIRROR (F8): when the daemon-fused presence (F5) reports
  // "away" AND there's been no local pointer/key activity for the idle window,
  // the dense HUD calms into a glanceable mirror. Local activity instantly wakes
  // it, so the mirror never appears while you're touching the machine.
  const lastActivityRef = useRef(Date.now());
  const [nowTick, setNowTick] = useState(() => new Date());
  useEffect(() => {
    const wake = () => {
      lastActivityRef.current = Date.now();
    };
    window.addEventListener("pointermove", wake, { passive: true });
    window.addEventListener("keydown", wake);
    const id = window.setInterval(() => setNowTick(new Date()), 1000);
    return () => {
      window.removeEventListener("pointermove", wake);
      window.removeEventListener("keydown", wake);
      window.clearInterval(id);
    };
  }, []);
  const atRest = isAtRest(
    state.presence?.state ?? null,
    lastActivityRef.current,
    nowTick.getTime(),
  );

  // Telemetry link: connect, parse, reduce. Auto-reconnect 1s -> 5s.
  // audio.level rms goes to the mutable audioStore (read by the Waveform
  // and CoreScene frame loops via refs) — the reducer also sees the
  // envelope for the listening/idle state machine but returns the same
  // reference when nothing render-visible changed, so 15Hz frames do NOT
  // re-render the React tree.
  useEffect(() => {
    const link = new TelemetryLink(TELEMETRY_URL, {
      onOpen: () => dispatch({ type: "ws.connected", at: Date.now() }),
      onClose: () => {
        audioStore.reset();
        dispatch({ type: "ws.disconnected", at: Date.now() });
      },
      onRaw: (raw) => {
        const envelope = parseEnvelope(raw);
        if (!envelope) return;
        if (envelope.event === "audio.level") {
          const rms = num(envelope.data, "rms");
          if (rms !== null) audioStore.push(rms, bool(envelope.data, "speaking") ?? false);
        }
        dispatch({ type: "telemetry", envelope, at: Date.now() });
      },
    });
    link.start();
    return () => link.stop();
  }, []);

  // Housekeeping tick: toast lifecycle + 12s stuck-state decay.
  useEffect(() => {
    const id = setInterval(() => dispatch({ type: "tick", at: Date.now() }), 250);
    return () => clearInterval(id);
  }, []);

  // F11 fullscreen.
  useEffect(() => bindFullscreenKey(), []);

  // DEPTH PARALLAX — publish the normalized cursor position (-1..1 from center)
  // as CSS vars on the root; background layers translate by it at different
  // depths (CSS), so the backdrop, conduits and core overlay separate in 3D as
  // the cursor moves. rAF-throttled, passive, and zero React churn (writes CSS
  // vars, never state). The R3F core does its own pointer tilt; panels stay put
  // (they're interactive). Motion is gated off in CSS under reduced-motion.
  useEffect(() => {
    if (typeof window === "undefined") return;
    let raf = 0;
    const onMove = (ev: PointerEvent) => {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(() => {
        const x = (ev.clientX / window.innerWidth - 0.5) * 2;
        const y = (ev.clientY / window.innerHeight - 0.5) * 2;
        const root = document.documentElement.style;
        root.setProperty("--par-x", x.toFixed(3));
        root.setProperty("--par-y", y.toFixed(3));
      });
    };
    window.addEventListener("pointermove", onMove, { passive: true });
    return () => {
      window.removeEventListener("pointermove", onMove);
      cancelAnimationFrame(raf);
    };
  }, []);

  // VOICE-REACTIVE LIGHT FIELD — a rAF loop reads the live mic/voice amplitude
  // from the audioStore (a ref, never React state) and publishes a smoothed
  // 0..1 `--voice` CSS var with an asymmetric attack/release ease, so the
  // ambient edge-bloom swells with speech and settles softly. Writes only a CSS
  // var → zero React re-renders (same anti-flash discipline as the core loop).
  // The visual is gated off in CSS under reduced-motion.
  useEffect(() => {
    if (typeof window === "undefined") return;
    let raf = 0;
    let v = 0;
    const tick = () => {
      const rms = audioStore.lastRms || 0;
      const target = Math.min(1, rms * 6);
      // gentle attack, slower release — tracks the loudness contour, not spikes
      v += (target - v) * (target > v ? 0.3 : 0.06);
      document.documentElement.style.setProperty("--voice", v.toFixed(3));
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, []);

  // ABOUT MENU — open the custom About panel when the user picks
  // "About J.A.R.V.I.S." from the native macOS menu. The Rust shell emits
  // `menu://about` with the bundle version as payload; we stash it and open the
  // panel. No-op (no native menu) outside the Tauri shell. Unsubscribe on unmount.
  useEffect(
    () =>
      onAboutMenu((v) => {
        if (v) setAppVersion(v);
        setAboutOpen(true);
      }),
    [],
  );

  // FIRST-RUN SETUP CHECK — resolve backend_installed() ONCE per launch, and ONLY
  // inside the real Tauri shell (a plain browser / vitest render has no backend to
  // install). The ref guard makes it idempotent across re-renders + the StrictMode
  // double mount. Outside the shell it stays null (the gate also requires
  // not-connected, so a browser render never shows the setup screen regardless).
  //
  // CONSERVATIVE: an unavailable/erroring check reads false from the bridge, but
  // the gate ALSO requires the command channel to be disconnected — so a running
  // daemon (channel connected) keeps the screen hidden even on a false-negative.
  useEffect(() => {
    if (backendCheckStarted.current) return;
    backendCheckStarted.current = true;
    if (!inTauri()) return; // shell-only: nothing to install in a browser/dev
    let cancelled = false;
    void (async () => {
      const installed = await backendInstalled();
      if (!cancelled) setBackendIsInstalled(installed);
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // LAUNCH AUTO-CHECK FOR UPDATES — fire ONCE per launch, and ONLY inside the
  // real Tauri shell (never a plain browser / vite dev / vitest render). The
  // ref guard makes it idempotent across re-renders + the StrictMode double
  // mount; the inTauri() gate keeps it from ever running outside the desktop
  // app (where checkForUpdates would only return "unavailable" anyway).
  //
  // HONESTY: we call checkForUpdates(false) (a CHECK, no install) and route the
  // result through decideLaunchUpdateAction, which produces a dialog/silent
  // outcome ONLY for status "available". Every other status
  // (not_configured / up_to_date / error / unavailable) yields "none" — we do
  // NOTHING visible at launch (no dialog, no nag), exactly today's quiet launch.
  // When available: pref ON -> SILENT install (with an honest non-blocking
  // toast) via the EXISTING signed backend command + relaunch; pref OFF -> open
  // the dialog naming the real version.
  useEffect(() => {
    if (launchUpdateChecked.current) return;
    launchUpdateChecked.current = true;
    if (!inTauri()) return; // shell-only: never in a browser / dev render
    let cancelled = false;
    void (async () => {
      const result = await checkForUpdates(false);
      if (cancelled) return;
      const action = decideLaunchUpdateAction(result, isAutoUpdateOn());
      if (action.kind === "none") return; // honest quiet launch
      if (action.kind === "dialog") {
        setUpdateDialogVersion(action.version);
        return;
      }
      // action.kind === "silent": pref is ON. Show a brief honest notice, then
      // install through the SAME signed backend command and relaunch — no
      // dialog, but never a silent surprise.
      dispatch({ type: "notice.toast", text: silentUpdateNotice(action.version), at: Date.now() });
      const installed = await checkForUpdates(true);
      if (cancelled) return;
      if (installed.status === "installed") {
        await relaunchApp();
        // If relaunch is unavailable the new binary still applies on the next
        // manual restart; we do not claim it finished.
      }
      // A non-"installed" result (e.g. an install error) is left to the manual
      // UpdatesSection on the next check — the launch path stays non-blocking
      // and never claims a success that did not happen.
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // "C" toggles the command deck (ignored while typing in a field, and never
  // stealing the settings modal's focus). Additive to the telemetry HUD.
  //
  // Esc is the ALWAYS-AVAILABLE KEYBOARD ESCAPE HATCH for takeover: when takeover
  // is active, Esc exits it FIRST (the takeover desktop has no OS chrome, so Esc
  // must always reach exitTakeover) before doing anything else. This is one of
  // the independent exits guaranteeing the operator is never locked out.
  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      const target = ev.target as HTMLElement | null;
      const typing =
        target &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.tagName === "SELECT" ||
          target.isContentEditable);
      if (takeoverActive && isExitKey(ev.key)) {
        // Highest-priority binding: Esc always leaves takeover.
        ev.preventDefault();
        exitTakeover();
        return;
      }
      if (!typing && (ev.key === "c" || ev.key === "C")) {
        ev.preventDefault();
        setDeckOpen((v) => !v);
      } else if (ev.key === "Escape") {
        setDeckOpen(false);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [takeoverActive, exitTakeover]);

  // FIRST-RUN SETUP gate (pure): show ONLY when in the shell, the backend is
  // KNOWN-not-installed, AND the daemon is not reachable. A connected daemon or a
  // resolved-installed backend (or an unresolved check) keeps it hidden.
  const showSetup = decideShowSetup({
    inTauri: inTauri(),
    installed: backendIsInstalled,
    connected: state.connected,
  });

  return (
    <div className="hud">
      {atRest && (
        <AmbientMode
          now={nowTick}
          presence={state.presence}
          briefCount={state.proactiveDigest?.items.length ?? 0}
          feedCount={Object.keys(state.appFeeds).length}
        />
      )}
      <div className="hud-backdrop" aria-hidden="true">
        <div className="watermark" />
      </div>
      <div className="core-layer">
        <CoreScene
          coreState={state.coreState}
          agentHue={state.activeAgent ? state.activeAgent.hue : null}
        />
      </div>
      {/* Ambient cyan aura breathing behind the core — painted on the DOM
          backdrop (not the opaque WebGL canvas) so the core reads as seated in
          light without touching the canvas anti-flash invariants. */}
      {/* Holographic projection floor: a perspective ground-grid receding to a
          horizon beneath the core, so it reads as projected in volumetric space.
          z:1 (deep background). */}
      <div className="holo-floor" aria-hidden="true" />
      <div className="core-aura" aria-hidden="true" />
      <div className="hud-vignette" />
      {/* Voice-reactive light field: ambient edge-bloom whose intensity follows
          the --voice amplitude var. z:2, pointer-events:none. */}
      <div className="voice-field" aria-hidden="true" />
      {/* Neural conduits: energy flowing from the core out to the panel columns.
          z:2, painted before the tactical overlay so the gauges sit on top. */}
      <ConduitField coreState={state.coreState} />
      {/* Tactical overlay framing the orb: live arc gauges + radar sweep +
          range rings + corner readouts. z:2, above the vignette, below panels. */}
      <CoreHud gauges={state.gauges} coreState={state.coreState} />

      {/* Always mounted; fade via CSS class so reconnect flaps cannot
          strobe the overlay (show transition is delayed ~500ms). */}
      <div className={`link-offline ${state.connected ? "" : "visible"}`}>
        <div className="pulse">
          <div className="big">LINK OFFLINE</div>
          <div className="small">ws://127.0.0.1:7177 — reconnecting</div>
        </div>
      </div>

      <div className={`banner ${state.inferenceOffline ? "visible" : ""}`}>
        <span className="pulse">LOCAL INFERENCE OFFLINE</span>
      </div>

      <AlertPanel alert={state.healAlert} onDismiss={() => dispatch({ type: "alert.dismiss" })} />
      <SelfHealPanel
        diagnosing={state.healDiagnosing}
        proposal={state.healProposal}
        onDismiss={() => dispatch({ type: "alert.dismiss" })}
      />
      <ForgePanel
        proposal={state.forgeProposal}
        alert={state.forgeAlert}
        onDismiss={() => dispatch({ type: "alert.dismiss" })}
      />

      <div
        className={`panel-layer${bootPlaying && !revealed ? " pre-reveal" : ""}`}
      >
        <ErrorBoundary label="status bar" resetKeys={[state.connected]}><StatusBar
          connected={state.connected}
          coreState={state.coreState}
          cloudKeyPresent={state.cloudKeyPresent}
          inferenceOffline={state.inferenceOffline}
          heal={state.heal}
          cloudModel={state.cloudModel}
          activeAgent={state.activeAgent}
          voiceId={state.voiceId}
          modelTier={state.modelTier}
          localWarm={state.localWarm}
          localTools={state.localTools}
          voiceTier={state.voiceTier}
          sttTier={state.sttTier}
          voiceMode={state.voiceMode}
          security={state.security}
          lockdown={state.lockdown}
          onPanic={() => void panic()}
          onOpenSettings={() => setSettingsTab("credentials")}
          onOpenDeck={() => setDeckOpen(true)}
        /></ErrorBoundary>
        <LatencyStrip timings={state.lastTimings} />
        <ErrorBoundary label="left column" resetKeys={[state.connected]}><div className="left-col">
          <ReticleDial cpuPercent={state.gauges.cpuPercent} coreState={state.coreState} />
          <TranscriptPanel lines={state.transcript} intent={state.lastIntent} />
          <AudioIoPanel audio={state.audioIo} />
        </div></ErrorBoundary>
        {/* Center column: the R3F core stays visible up top; the intel-feed
            panel slots into the lower half (CSS anchors it to the bottom and
            caps its height so the core sphere is never occluded). */}
        <ErrorBoundary label="center column" resetKeys={[state.connected]}><div className="center-col">
          <GlobalScanPanel
            feed={state.appFeeds[GLOBAL_SCAN_APP]}
            running={state.runningApps.has(GLOBAL_SCAN_APP)}
          />
        </div></ErrorBoundary>
        <ErrorBoundary label="right column" resetKeys={[state.connected]}><div className="right-col">
          <AgentPanel active={state.activeAgent} />
          <SuggestionsPanel
            suggestions={state.suggestions}
            onAccept={acceptSuggestion}
            onDismiss={dismissSuggestion}
          />
          <BriefFocusPanel digest={state.proactiveDigest} focus={state.focusProfile} />
          <PresencePanel presence={state.presence} />
          <VisionPanel
            feed={state.appFeeds[VISION_APP]}
            running={state.runningApps.has(VISION_APP)}
            describe={state.visionDescribe}
            soundMonitor={state.audioSoundMonitor}
            screenContext={state.screenContext}
          />
          <ImagePanel generated={state.imageGenerated} />
          <SiliconCanvasPanel
            feed={state.appFeeds[SILICON_CANVAS_APP]}
            running={state.runningApps.has(SILICON_CANVAS_APP)}
          />
          <NexusPanel
            feed={state.appFeeds[NEXUS_APP_NAME]}
            running={state.runningApps.has(NEXUS_APP_NAME)}
          />
          <MarkForgePanel
            feed={state.appFeeds[MARK_FORGE_APP_NAME]}
            running={state.runningApps.has(MARK_FORGE_APP_NAME)}
          />
          <McpPanel mcp={state.mcp} />
          <CapabilityStatusPanel map={state.capabilityMap} />
          <CapabilityAtlasPanel atlas={state.capabilityAtlas} />
          <AppDeckPanel
            runningApps={state.runningApps}
            appFeeds={state.appFeeds}
            manifestIssues={state.appManifestIssues}
          />
          <AttributionHealthPanel health={state.attributionHealth} />
          <TccSentinelPanel sentinel={state.tccSentinel} anomalies={state.tccAnomalies} />
          <IntrospectPanel
            status={state.introspect}
            alerts={state.introspectAlerts}
            capabilities={state.introspectCapabilities}
          />
          <ConnectorMarketplacePanel mcp={state.mcp} />
          <ExtensibilityPanel webhooks={state.webhooks} plugins={state.plugins} />
          <AuditPanel audit={state.audit} liveGate={state.liveGate} />
          <ActionPanel action={state.actionSurface} />
          <DocSearchPanel
            index={state.docIndex}
            search={state.docSearch}
            pdfJail={state.pdfJailAvailable}
          />
          <CodeIntelPanel code={state.codeIntel} />
          <ShellPanel shell={state.shell} />
          <UiActuatePanel uiActuate={state.uiActuate} />
          <UnifiedSearchPanel result={state.unifiedSearch} />
          <KnowledgeGraphPanel graph={state.knowledgeGraph} />
          <AnswerSourcesPanel annotation={state.answerAnnotation} />
          <VerifyPanel status={state.verifyStatus} />
          <AnswerCrossCheckPanel
            crossCheck={state.crossCheckStatus}
            debate={state.debateStatus}
          />
          <ResearchNotebooksPanel activity={state.notebook} />
          <ReportPanel report={state.report} />
          <ChartPanel spec={state.chart} />
          <LifeLogPanel digest={state.lifelog} />
          <MemoryPanel memory={state.memory} />
          <EvalPanel report={state.evalReport} proposal={state.optimizerProposal} />
          <SkillsPanel skills={state.skills} />
          <InferencePerfPanel perf={state.inferencePerf} />
          <DiagnosticsPanel gauges={state.gauges} facts={state.facts} actions={state.actions} />
        </div></ErrorBoundary>
        <Waveform connected={state.connected} />
      </div>

      {/* ENTER TAKEOVER — the EXPLICIT operator trigger. Nothing auto-enters
          takeover; this is the only entry point and it ships visible-but-idle.
          Hidden while already in takeover (the stage owns the EXIT control). */}
      {!takeoverActive && (
        <button
          type="button"
          className="takeover-enter"
          onClick={enterTakeover}
          aria-label="Enter takeover"
          title="Promote the HUD to a full-desktop kiosk takeover (Esc or EXIT TAKEOVER to leave)"
        >
          ⤢ ENTER TAKEOVER
        </button>
      )}

      <Toasts toasts={state.toasts} />
      <CommandDeck open={deckOpen} onClose={() => setDeckOpen(false)} />

      {/* CINEMATIC BOOT / WAKE REVEAL — mounts once on first connect, above
          everything (z 9999), and unmounts itself when the sequence ends. */}
      {bootPlaying && <BootReveal onReveal={revealPanels} onDone={dismissBoot} />}

      {/* FIRST-RUN ONBOARDING — shown once (real persisted flag); routes to the
          existing, already-gated Settings surfaces and never bypasses them. */}
      {onboardingOpen && (
        <OnboardingWizard onRoute={onboardingRoute} onDismiss={dismissOnboarding} />
      )}

      {settingsOpen && (
        <SettingsModal
          mcp={state.mcp}
          voiceId={state.voiceId}
          modelTier={state.modelTier}
          sttTier={state.sttTier}
          docIndex={state.docIndex}
          policy={state.policy}
          security={state.security}
          lockdown={state.lockdown}
          onLockedChange={onLockedChange}
          initialTab={settingsTab ?? "credentials"}
          onReopenOnboarding={reopenOnboarding}
          onClose={() => setSettingsTab(null)}
        />
      )}

      {/* LAUNCH UPDATE DIALOG — mounts ONLY when the launch auto-check reported a
          REAL available update (status "available") and the auto-update pref is
          OFF. Cancel just closes (pref unchanged, re-checks next launch); the
          install path runs through the existing signed backend command. */}
      {updateDialogVersion !== null && (
        <UpdateDialog
          version={updateDialogVersion}
          onClose={() => setUpdateDialogVersion(null)}
        />
      )}

      {/* ABOUT PANEL — the custom "About J.A.R.V.I.S." panel. Mounts when the
          native "About" menu item is picked (App listens for menu://about). It
          carries the version, a working Check-for-Updates button, and the
          credit. A real available version routes to the signed UpdateDialog. */}
      {aboutOpen && (
        <AboutPanel
          version={appVersion}
          onClose={() => setAboutOpen(false)}
          onUpdateAvailable={(v) => {
            setAboutOpen(false);
            setUpdateDialogVersion(v);
          }}
        />
      )}

      {/* TAKEOVER STAGE — the full-desktop holographic layout. Mounts ONLY when
          takeover is active (windowed HUD above is the default), and always
          renders the visible EXIT control inside, so exit is never hideable. */}
      {takeoverActive && <TakeoverStage state={state} onExit={exitTakeover} />}

      {/* FIRST-RUN SETUP — the install gate for a freshly-downloaded app. Mounts
          ONLY when (in the Tauri shell) the backend is genuinely NOT installed
          AND the daemon is not reachable; NEVER when JARVIS is installed +
          running (decideShowSetup). It opens Terminal on the real install.sh and
          auto-dismisses when the daemon connects (state.connected flips true). */}
      {showSetup && <FirstRunSetup connected={state.connected} />}
    </div>
  );
}
