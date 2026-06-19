import { useCallback, useEffect, useReducer, useRef, useState } from "react";
import ActionPanel from "./components/ActionPanel";
import AgentPanel from "./components/AgentPanel";
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
import ForgePanel from "./components/ForgePanel";
import GlobalScanPanel, { GLOBAL_SCAN_APP } from "./components/GlobalScanPanel";
import ImagePanel from "./components/ImagePanel";
import InferencePerfPanel from "./components/InferencePerfPanel";
import KnowledgeGraphPanel from "./components/KnowledgeGraphPanel";
import LatencyStrip from "./components/LatencyStrip";
import LifeLogPanel from "./components/LifeLogPanel";
import MarkForgePanel, { MARK_FORGE_APP_NAME } from "./components/MarkForgePanel";
import McpPanel from "./components/McpPanel";
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
import { bindFullscreenKey, checkForUpdates, inTauri, relaunchApp } from "./tauri/bridge";
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

  return (
    <div className="hud">
      <div className="hud-backdrop" aria-hidden="true">
        <div className="watermark" />
      </div>
      <div className="core-layer">
        <CoreScene
          coreState={state.coreState}
          agentHue={state.activeAgent ? state.activeAgent.hue : null}
        />
      </div>
      <div className="hud-vignette" />

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

      <div className="panel-layer">
        <StatusBar
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
        />
        <LatencyStrip timings={state.lastTimings} />
        <div className="left-col">
          <ReticleDial cpuPercent={state.gauges.cpuPercent} coreState={state.coreState} />
          <TranscriptPanel lines={state.transcript} intent={state.lastIntent} />
          <AudioIoPanel audio={state.audioIo} />
        </div>
        {/* Center column: the R3F core stays visible up top; the intel-feed
            panel slots into the lower half (CSS anchors it to the bottom and
            caps its height so the core sphere is never occluded). */}
        <div className="center-col">
          <GlobalScanPanel
            feed={state.appFeeds[GLOBAL_SCAN_APP]}
            running={state.runningApps.has(GLOBAL_SCAN_APP)}
          />
        </div>
        <div className="right-col">
          <AgentPanel active={state.activeAgent} />
          <SuggestionsPanel
            suggestions={state.suggestions}
            onAccept={acceptSuggestion}
            onDismiss={dismissSuggestion}
          />
          <BriefFocusPanel digest={state.proactiveDigest} focus={state.focusProfile} />
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
          <ExtensibilityPanel webhooks={state.webhooks} plugins={state.plugins} />
          <AuditPanel audit={state.audit} liveGate={state.liveGate} />
          <ActionPanel action={state.actionSurface} />
          <DocSearchPanel index={state.docIndex} search={state.docSearch} />
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
        </div>
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

      {/* TAKEOVER STAGE — the full-desktop holographic layout. Mounts ONLY when
          takeover is active (windowed HUD above is the default), and always
          renders the visible EXIT control inside, so exit is never hideable. */}
      {takeoverActive && <TakeoverStage state={state} onExit={exitTakeover} />}
    </div>
  );
}
