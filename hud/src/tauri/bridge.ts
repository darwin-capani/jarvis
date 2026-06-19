/**
 * Thin, guarded wrappers around the Tauri shell. Every call degrades
 * gracefully when the frontend runs in a plain browser (vite dev) where no
 * Tauri runtime exists.
 *
 * The API key value passes through here exactly once on save and is never
 * logged, stored, or echoed back.
 */
import { invoke } from "@tauri-apps/api/core";
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
  if (!inTauri()) throw new Error("settings require the JARVIS shell");
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
  if (!inTauri()) throw new Error("settings require the JARVIS shell");
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

/** CHECK for a JARVIS update via the updater plugin. When `install` is true and a
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
      detail: "Updates are checked from the JARVIS desktop app.",
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
      detail: "Uninstall runs from the JARVIS desktop app (it opens Terminal on uninstall.sh).",
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
