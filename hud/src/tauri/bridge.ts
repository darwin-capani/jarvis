/**
 * Thin, guarded wrappers around the Tauri shell. Every call degrades
 * gracefully when the frontend runs in a plain browser (vite dev) where no
 * Tauri runtime exists.
 *
 * The API key value passes through here exactly once on save and is never
 * logged, stored, or echoed back.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

export function inTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

import type { VerifyResult, VerifyStatus } from "../core/credentials";

export type { VerifyResult, VerifyStatus };

/** Presence check for one credential account — "on file" presence only. */
export async function keychainStatus(account: string): Promise<boolean> {
  if (!inTauri()) return false;
  return invoke<boolean>("keychain_status", { account });
}

/** Remove the stored secret for one credential account. */
export async function keychainDelete(account: string): Promise<void> {
  if (!inTauri()) throw new Error("settings require the DARWIN shell");
  await invoke("keychain_delete", { account });
}

/**
 * Store a secret directly under one allowlisted Keychain account, WITHOUT a live
 * verification round-trip. Used for credentials that have no paste-time verify
 * endpoint — notably an MCP server token (`mcp_<server>_token`), which is an
 * opaque per-server value the daemon resolves at call time. The shell guards the
 * account against the same allowlist as every other write (a non-allowlisted
 * account is rejected), so this can never write an arbitrary item. The secret
 * passes through here exactly once and is never logged or echoed back.
 */
export async function keychainSet(account: string, secret: string): Promise<void> {
  if (!inTauri()) throw new Error("settings require the DARWIN shell");
  await invoke("keychain_set", { account, secret });
}

/**
 * Verify a candidate secret for `id` (live HTTP round-trip in the shell) AND,
 * only when valid, store it in the Keychain under the registered account. The
 * Enter-key action. An unverified secret is never stored. The secret passes
 * through here exactly once and is never logged or echoed back.
 */
export async function verifyAndStore(id: string, secret: string): Promise<VerifyResult> {
  if (!inTauri()) {
    return { status: "network_error", detail: "no shell", stored: false };
  }
  return invoke<VerifyResult>("verify_and_store", { id, secret });
}

/** Verify a candidate secret for `id` WITHOUT storing it. */
export async function verifyCredential(id: string, secret: string): Promise<VerifyResult> {
  if (!inTauri()) {
    return { status: "network_error", detail: "no shell" };
  }
  return invoke<VerifyResult>("verify_credential", { id, secret });
}

/* --------------------------------------------------- Google connect (OAuth) */

/** Outcome of asking the shell to connect Google. The browser consent + loopback
 *  run inside the DAEMON at runtime — `connected` is true ONLY when a refresh
 *  token is genuinely on file, never faked. */
export interface ConnectResult {
  ready: boolean;
  connected: boolean;
  detail: string;
}

/** Begin the Google connect flow. In a plain browser there is no shell — return
 *  a not-ready result with guidance rather than pretending to connect. */
export async function beginGoogleAuth(): Promise<ConnectResult> {
  if (!inTauri()) {
    return {
      ready: false,
      connected: false,
      detail: "Connecting Google is available in the desktop app.",
    };
  }
  return invoke<ConnectResult>("begin_google_auth", {});
}

/** Begin the X (Twitter) connect flow. The browser consent + loopback run in
 *  the daemon's `connect_x` tool — `connected` is true ONLY when a refresh
 *  token is genuinely on file, never faked. */
export async function beginXAuth(): Promise<ConnectResult> {
  if (!inTauri()) {
    return {
      ready: false,
      connected: false,
      detail: "Connecting X is available in the desktop app.",
    };
  }
  return invoke<ConnectResult>("begin_x_auth", {});
}

/** Begin the LinkedIn connect flow. The browser consent + loopback run in the
 *  daemon's `connect_linkedin` tool — `connected` is true ONLY when a refresh
 *  token is genuinely on file, never faked. */
export async function beginLinkedinAuth(): Promise<ConnectResult> {
  if (!inTauri()) {
    return {
      ready: false,
      connected: false,
      detail: "Connecting LinkedIn is available in the desktop app.",
    };
  }
  return invoke<ConnectResult>("begin_linkedin_auth", {});
}

/** Begin the Google Ads connect flow. A SEPARATE connection from Workspace; the
 *  browser consent + loopback run in the daemon's `connect_google_ads` tool —
 *  `connected` is true ONLY when the (Ads) refresh token is genuinely on file. */
export async function beginGoogleAdsAuth(): Promise<ConnectResult> {
  if (!inTauri()) {
    return {
      ready: false,
      connected: false,
      detail: "Connecting Google Ads is available in the desktop app.",
    };
  }
  return invoke<ConnectResult>("begin_google_ads_auth", {});
}

/** Begin the Meta Ads connect flow. The browser consent + the short->long token
 *  exchange run in the daemon's `connect_meta_ads` tool — `connected` is true
 *  ONLY when the long-lived token is genuinely on file, never faked. */
export async function beginMetaAdsAuth(): Promise<ConnectResult> {
  if (!inTauri()) {
    return {
      ready: false,
      connected: false,
      detail: "Connecting Meta Ads is available in the desktop app.",
    };
  }
  return invoke<ConnectResult>("begin_meta_ads_auth", {});
}

/** Begin the WHOOP connect flow. The browser consent + loopback run in the
 *  daemon's `connect_whoop` tool — `connected` is true ONLY when a refresh token
 *  is genuinely on file, never faked. */
export async function beginWhoopAuth(): Promise<ConnectResult> {
  if (!inTauri()) {
    return {
      ready: false,
      connected: false,
      detail: "Connecting WHOOP is available in the desktop app.",
    };
  }
  return invoke<ConnectResult>("begin_whoop_auth", {});
}

/** The connect function for an OAuth connection-STATUS credential id. Unknown
 *  ids fall back to a not-ready result so the row never claims a connection. */
export function beginAuthForId(id: string): () => Promise<ConnectResult> {
  switch (id) {
    case "google_workspace":
      return beginGoogleAuth;
    case "x_social":
      return beginXAuth;
    case "linkedin_social":
      return beginLinkedinAuth;
    case "google_ads":
      return beginGoogleAdsAuth;
    case "meta_ads":
      return beginMetaAdsAuth;
    case "whoop":
      return beginWhoopAuth;
    default:
      return async () => ({
        ready: false,
        connected: false,
        detail: "No connect flow for this credential.",
      });
  }
}

/* ----------------------------------------------------- self-heal GUI apply */

/** The staged artifacts of a heal proposal, for human review in the modal. */
export interface HealProposalDetail {
  diff: string;
  report: string;
}

/** Result of the gated apply script. `ok` is true only when every gate passed,
 *  the patch was applied and the release rebuilt. `available` is false in a
 *  plain browser (no Tauri shell) — Accept then shows a "desktop app" state
 *  rather than throwing. */
export interface HealApplyResult {
  ok: boolean;
  stage: string;
  log: string;
  /** False only when invoked outside the Tauri shell (browser dev). */
  available: boolean;
}

/** Read the staged diff + report for `ts` for HUMAN REVIEW. In a plain browser
 *  there is no shell to read the repo — return empty artifacts with a marker so
 *  the panel can say the diff is only viewable in the desktop app. */
export async function healProposalDetail(ts: string): Promise<HealProposalDetail> {
  if (!inTauri()) {
    return {
      diff: "",
      report: "(diff review and apply are available in the desktop app)",
    };
  }
  return invoke<HealProposalDetail>("heal_proposal_detail", { ts });
}

/** Run the gated apply script for `ts` (re-validates, then applies on green).
 *  In a plain browser there is no shell — return available:false so the UI can
 *  surface "available in the desktop app" instead of throwing. */
export async function healApply(ts: string): Promise<HealApplyResult> {
  if (!inTauri()) {
    return {
      ok: false,
      stage: "unavailable",
      log: "Accept & apply runs the gated re-validation script — available in the desktop app only.",
      available: false,
    };
  }
  const r = await invoke<{ ok: boolean; stage: string; log: string }>("heal_apply", { ts });
  return { ...r, available: true };
}

/* --------------------------------------------------------- auto-updates (WS4a) */

/** The honest outcome of an update check (mirrors the Rust `UpdateCheck`).
 *  `status` is the small contract vocabulary the UI switches on; `detail` is a
 *  short human line (never a secret); `version` is present only when an update is
 *  available/installed. */
export interface UpdateCheck {
  /** "not_configured" (the shipped state — no owner key/release yet), "up_to_date",
   *  "available", "installed", "error", or "unavailable" (no desktop shell). */
  status:
    | "not_configured"
    | "up_to_date"
    | "available"
    | "installed"
    | "error"
    | "unavailable";
  detail: string;
  version?: string | null;
}

/** CHECK for a DARWIN update via the updater plugin. When `install` is true and a
 *  newer SIGNED version exists, it is downloaded, signature-verified against the
 *  owner's public key, and installed. In a plain browser (no shell) there is
 *  nothing to update — return an honest unavailable result rather than throwing.
 *
 *  HONEST: while the owner has not yet added their updater public key + published
 *  a signed release, the backend returns `not_configured` with NO network call —
 *  it never pretends an update exists. */
export async function checkForUpdates(install = false): Promise<UpdateCheck> {
  if (!inTauri()) {
    return {
      status: "unavailable",
      detail: "Updates are checked from the DARWIN desktop app.",
    };
  }
  try {
    return await invoke<UpdateCheck>("check_for_updates", { install });
  } catch (e) {
    return {
      status: "error",
      detail: typeof e === "string" ? e : "update check failed",
    };
  }
}

/** RELAUNCH the app to finish applying an installed update. Drives the
 *  `relaunch_app` backend command, which calls Tauri's built-in
 *  `AppHandle::restart()` (no new install authority, no plugin) — it merely
 *  restarts the already-installed binary. In a plain browser there is no shell
 *  to relaunch — return false so the UI can fall back to an honest
 *  "Restart to finish updating" state rather than throwing. */
export async function relaunchApp(): Promise<boolean> {
  if (!inTauri()) return false;
  try {
    await invoke("relaunch_app");
    // restart() does not return (the process is replaced); reaching here means
    // the call dispatched without throwing.
    return true;
  } catch {
    return false;
  }
}

/* ---------------------------------------------------- About menu (native) */

/** The native-menu event the Rust shell emits when the user picks
 *  "About D.A.R.W.I.N." from the macOS app menu. The payload is the app version
 *  string (from the bundle), so the panel always shows the real version. */
export const ABOUT_MENU_EVENT = "menu://about";

/** Subscribe to the native "About D.A.R.W.I.N." menu click. The Rust shell
 *  replaces the system about panel with a custom menu item that emits
 *  `menu://about` carrying the app version; this opens our custom About panel
 *  (which has the working "Check for Updates" button + the credit). Returns an
 *  unsubscribe fn. In a plain browser (no shell) there is no native menu — this
 *  is a no-op that returns a no-op unsubscribe, so callers need no guard. */
export function onAboutMenu(cb: (version: string) => void): () => void {
  if (!inTauri()) return () => {};
  const unlisten = listen<string>(ABOUT_MENU_EVENT, (e) => cb(e.payload || ""));
  return () => {
    void unlisten.then((un) => un()).catch(() => {});
  };
}

/* ----------------------------------------------------------- uninstall (WS4a) */

/** The honest outcome of opening the uninstaller (mirrors the Rust
 *  `UninstallOpen`). `opened` is true only when Terminal.app was launched on the
 *  installed uninstall.sh; `detail` is a short human line (never a secret). */
export interface UninstallOpen {
  opened: boolean;
  detail: string;
}

/** OPEN Terminal.app running the installed `uninstall.sh`. This does NOT run the
 *  uninstaller and passes NO auto-confirm — the user completes the script's TWO
 *  typed confirmations in the terminal. In a plain browser there is no shell —
 *  return an honest not-opened result rather than throwing. */
export async function uninstallOpen(): Promise<UninstallOpen> {
  if (!inTauri()) {
    return {
      opened: false,
      detail: "Uninstall runs from the DARWIN desktop app (it opens Terminal on uninstall.sh).",
    };
  }
  try {
    return await invoke<UninstallOpen>("uninstall_open");
  } catch (e) {
    return {
      opened: false,
      detail: typeof e === "string" ? e : "could not open the uninstaller",
    };
  }
}

/* ------------------------------------------------------ first-run setup */

/** The honest outcome of opening the installer (mirrors the Rust `SetupOpen`).
 *  `opened` is true only when Terminal.app was launched on the public install.sh
 *  one-liner; `detail` is a short human line (never a secret — only a public URL). */
export interface SetupOpen {
  opened: boolean;
  detail: string;
}

/** HONEST presence check of the installed DARWIN backend. Drives the FIRST-RUN
 *  SETUP gate. Returns true ONLY when the install home
 *  (`~/Library/Application Support/DARWIN`) has BOTH the built daemon binary AND
 *  the venv python. In a plain browser there is no shell + no install to check —
 *  return false (the gate then ALSO requires not-connected, so a browser/dev
 *  render never shows the setup screen anyway). A thrown shell error also reads
 *  false: when unsure, we never nag. */
export async function backendInstalled(): Promise<boolean> {
  if (!inTauri()) return false;
  try {
    return await invoke<boolean>("backend_installed", {});
  } catch {
    return false;
  }
}

/** OPEN Terminal.app running the public `install.sh` one-liner
 *  (`curl -fsSL …/install.sh | bash -s -- -y`). This REUSES the real working
 *  installer; it does NOT reimplement provisioning. Terminal is required so the
 *  Homebrew sudo prompt has a tty. In a plain browser there is no shell — return
 *  an honest not-opened result rather than throwing. */
export async function openSetupInstall(): Promise<SetupOpen> {
  if (!inTauri()) {
    return {
      opened: false,
      detail: "Installing DARWIN runs from the desktop app (it opens Terminal on install.sh).",
    };
  }
  try {
    return await invoke<SetupOpen>("open_setup_install", {});
  } catch (e) {
    return {
      opened: false,
      detail: typeof e === "string" ? e : "could not open the installer",
    };
  }
}

/* ------------------------------------------------------- SFX cues (Phase-2) */

/** The honest outcome of a play-cue attempt (mirrors the Rust `PlayCueReply`).
 *  `outcome` is the small contract vocabulary the UI maps to prose; it NEVER
 *  claims a cue played when the daemon reported a silent no-op. `detail` is a
 *  short human line (never a secret, never the produced WAV path). */
export interface PlayCueReply {
  /** "played" | "cached" | "disabled" | "unknown" | "failed" | "no_shell". */
  outcome:
    | "played"
    | "cached"
    | "disabled"
    | "unknown"
    | "failed"
    | "no_shell";
  detail: string;
}

/**
 * Play a built-in SFX cue by NAME via the `play_sfx_cue` backend command. This
 * adds NO authority and NO new tier: the backend relays a bounded `play_cue`
 * request through the SAME token-injecting command socket the deck uses, and the
 * daemon's already-shipped gate (`[voice].cloud_sfx` + an ElevenLabs key, online)
 * makes the real, honest call. With the switch off / no key / offline the daemon
 * returns an honest silent no-op (`disabled`) — never a fabricated cue.
 *
 * In a plain browser (no shell) there is nothing to play — return an honest
 * `no_shell` outcome rather than throwing. The cue NAME is the only thing that
 * leaves the UI here; no key value and no path ever cross this boundary.
 */
export async function playSfxCue(cue: string): Promise<PlayCueReply> {
  if (!inTauri()) {
    return {
      outcome: "no_shell",
      detail: "Playing cues requires the DARWIN desktop app.",
    };
  }
  try {
    return await invoke<PlayCueReply>("play_sfx_cue", { cue });
  } catch (e) {
    return {
      outcome: "failed",
      detail: typeof e === "string" ? e : "could not play the cue",
    };
  }
}

/* ------------------------------------------------------- Voice Lab (Phase-2) */

/** The honest outcome of a Voice-Lab attempt (mirrors the Rust `VoiceLabReply`).
 *  `outcome` is the small contract vocabulary the UI maps to prose; it NEVER
 *  claims a voice/dictionary was created when the daemon reported an honest `Err`.
 *  `detail` is a short human line (never a secret, never a voice/dictionary id). */
export interface VoiceLabReply {
  /** "created" | "unavailable" | "failed" | "no_shell". */
  outcome: "created" | "unavailable" | "failed" | "no_shell";
  detail: string;
}

/**
 * DESIGN an ElevenLabs voice for `agent` from a text `description` (+ an optional
 * display `name`) via the `design_voice` backend command. This adds NO authority
 * and NO new tier: the backend relays a bounded `design_voice` request through the
 * SAME token-injecting command socket the deck uses, and the daemon's
 * already-shipped gate (a key on file + a non-Local tier) makes the real, honest
 * call. With the tier off / no key / offline the daemon returns an honest `Err`
 * (`unavailable`) — never a fabricated voice.
 *
 * In a plain browser (no shell) there is nothing to design — return an honest
 * `no_shell` outcome rather than throwing. Only the text description (+ the agent /
 * name labels) leaves the UI here; no key value and no id ever cross this boundary.
 */
export async function designVoice(
  agent: string,
  description: string,
  name?: string,
): Promise<VoiceLabReply> {
  if (!inTauri()) {
    return {
      outcome: "no_shell",
      detail: "Designing a voice requires the DARWIN desktop app.",
    };
  }
  try {
    return await invoke<VoiceLabReply>("design_voice", { agent, description, name });
  } catch (e) {
    return {
      outcome: "failed",
      detail: typeof e === "string" ? e : "could not design the voice",
    };
  }
}

/**
 * ADD a single-alias pronunciation rule (`word` -> `say`, under an optional
 * dictionary `name`) via the `create_pronunciation` backend command. Same shape +
 * honesty as {@link designVoice}: the backend relays a bounded
 * `create_pronunciation` request through the SAME token-injecting socket; the
 * daemon's already-shipped gate makes the real call and returns an honest `Err`
 * (`unavailable`) when the tier is off / no key / offline — never a fabricated
 * dictionary.
 *
 * In a plain browser (no shell) there is nothing to create — return an honest
 * `no_shell` outcome. Text rules only; no audio, key value, or id ever crosses.
 */
export async function createPronunciation(
  word: string,
  say: string,
  name?: string,
): Promise<VoiceLabReply> {
  if (!inTauri()) {
    return {
      outcome: "no_shell",
      detail: "Adding a pronunciation requires the DARWIN desktop app.",
    };
  }
  try {
    return await invoke<VoiceLabReply>("create_pronunciation", { word, say, name });
  } catch (e) {
    return {
      outcome: "failed",
      detail: typeof e === "string" ? e : "could not add the pronunciation",
    };
  }
}

/* ------------------------------------------------------ Compose music (Phase-3) */

/** The honest outcome of a Compose-music attempt (mirrors the Rust `MusicReply`).
 *  `outcome` is the small contract vocabulary the UI maps to prose; it NEVER
 *  claims a track was composed when the daemon reported an honest no-op / failure.
 *  `detail` is a short human line (never a secret, never the produced audio path). */
export interface MusicReply {
  /** "created" | "unavailable" | "failed" | "no_shell". */
  outcome: "created" | "unavailable" | "failed" | "no_shell";
  detail: string;
}

/**
 * COMPOSE a full music track from a text `prompt` (+ an OPTIONAL `lengthMs`) via
 * the `compose_music` backend command. This adds NO authority and NO new tier:
 * the backend relays a bounded `compose_music` request through the SAME
 * token-injecting command socket the deck uses, and the daemon's already-shipped
 * gate (`[voice].cloud_music` + an ElevenLabs key on file + a non-Local tier)
 * makes the real, honest call. With the switch off / no key / offline the daemon
 * returns an honest "unavailable" / "didn't go through, nothing was created" prose
 * (`unavailable` / `failed`) — never a fabricated track.
 *
 * In a plain browser (no shell) there is nothing to compose — return an honest
 * `no_shell` outcome rather than throwing. Only the text prompt (+ the optional
 * length) leaves the UI here; no key value and no audio path ever cross this
 * boundary.
 */
export async function composeMusic(
  prompt: string,
  lengthMs?: number,
): Promise<MusicReply> {
  if (!inTauri()) {
    return {
      outcome: "no_shell",
      detail: "Composing music requires the DARWIN desktop app.",
    };
  }
  try {
    return await invoke<MusicReply>("compose_music", { prompt, lengthMs });
  } catch (e) {
    return {
      outcome: "failed",
      detail: typeof e === "string" ? e : "could not compose the track",
    };
  }
}

export async function toggleFullscreen(): Promise<void> {
  if (!inTauri()) {
    // Browser fallback for development.
    if (document.fullscreenElement) await document.exitFullscreen();
    else await document.documentElement.requestFullscreen().catch(() => {});
    return;
  }
  const win = getCurrentWindow();
  const full = await win.isFullscreen();
  await win.setFullscreen(!full);
}

/** F11 -> fullscreen toggle. Returns an unsubscribe function. */
export function bindFullscreenKey(): () => void {
  const onKey = (ev: KeyboardEvent) => {
    if (ev.key === "F11") {
      ev.preventDefault();
      void toggleFullscreen();
    }
  };
  window.addEventListener("keydown", onKey);
  return () => window.removeEventListener("keydown", onKey);
}

/* ---------------------------------------------------- SYSTEM ACCESS (macOS TCC) */

/** Outcome of opening a macOS System Settings privacy pane. `opened` is true
 *  only when System Settings was actually launched; `detail` is a short, honest
 *  human line (never a faked "granted"). */
export interface PaneOpenResult {
  opened: boolean;
  label: string;
  detail: string;
}

/**
 * Open ONE macOS Privacy pane by an allowlisted KEY (from core/permissions.ts).
 * The backend maps the key to a fixed System Settings anchor and opens it — the
 * frontend never passes a URL, so an arbitrary URL can never be opened. Degrades
 * honestly in the browser (no shell to open System Settings).
 */
export async function openPrivacyPane(pane: string): Promise<PaneOpenResult> {
  if (!inTauri()) {
    return {
      opened: false,
      label: pane,
      detail: "Opening System Settings needs the DARWIN desktop app.",
    };
  }
  try {
    return await invoke<PaneOpenResult>("open_privacy_pane", { pane });
  } catch (e) {
    return {
      opened: false,
      label: pane,
      detail: typeof e === "string" ? e : "could not open System Settings",
    };
  }
}

/**
 * REQUEST a permission the right way: fire the NATIVE macOS prompt from the app
 * bundle (a clean "DARWIN" dialog) for permissions that have a request API; the
 * backend falls back to opening the exact Settings pane when macOS won't prompt
 * (already decided, or no API like Full Disk Access). Honest browser fallback.
 */
export async function requestAccess(pane: string): Promise<PaneOpenResult> {
  if (!inTauri()) {
    return {
      opened: false,
      label: pane,
      detail: "Requesting permissions needs the DARWIN desktop app.",
    };
  }
  try {
    return await invoke<PaneOpenResult>("request_access", { pane });
  } catch (e) {
    return {
      opened: false,
      label: pane,
      detail: typeof e === "string" ? e : "could not request the permission",
    };
  }
}

/**
 * The "re-request all permissions" action: opens System Settings → Privacy &
 * Security (every category listed). Takes no argument, so there is no injection
 * surface at all. Honest browser fallback.
 */
export async function requestAllPermissions(): Promise<PaneOpenResult> {
  if (!inTauri()) {
    return {
      opened: false,
      label: "Privacy & Security",
      detail: "Opening System Settings needs the DARWIN desktop app.",
    };
  }
  try {
    return await invoke<PaneOpenResult>("request_all_permissions", {});
  } catch (e) {
    return {
      opened: false,
      label: "Privacy & Security",
      detail: typeof e === "string" ? e : "could not open System Settings",
    };
  }
}
