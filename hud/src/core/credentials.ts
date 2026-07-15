/**
 * Credential registry — the single source of truth for the settings panel,
 * mirrored byte-for-byte against the Rust allowlist in
 * `src-tauri/src/credentials.rs` (CONTRACT part A). Anything stored in the
 * Keychain by the HUD must have an entry here AND in the Rust registry; the
 * backend rejects any `account` not on its allowlist, so the frontend cannot
 * write arbitrary Keychain items.
 *
 * This module is PURE — no DOM, no Tauri, no React — so it is unit-testable
 * headlessly under vitest (node env).
 *
 * NB: the Anthropic account string is EXACTLY "anthropic_api_key" because
 * darwind reads that item at startup. Do not rename it.
 */

/** "bearer": paste a token + verify over HTTP. "oauth": a connection-STATUS row
 *  whose secret (the refresh token) is written by the daemon after a browser
 *  consent flow — never pasted, never verifiable by a bare HTTP call. */
export type CredentialKind = "bearer" | "oauth";

export interface Credential {
  /** Stable id used to dispatch verification (mirrors the Rust id->account map). */
  id: string;
  /** Human label shown in the panel. */
  label: string;
  /** macOS Keychain account string under service com.darwin.daemon. */
  keychain_account: string;
  kind: CredentialKind;
}

/**
 * v1 credential set. ORDER is the render order. Keep this lockstep with
 * `CREDENTIALS` in src-tauri/src/credentials.rs.
 */
export const CREDENTIALS: readonly Credential[] = [
  {
    id: "anthropic",
    label: "Anthropic API Key",
    keychain_account: "anthropic_api_key",
    kind: "bearer",
  },
  {
    id: "github",
    label: "GitHub Token (PAT)",
    keychain_account: "github_pat",
    kind: "bearer",
  },
  {
    id: "slack",
    label: "Slack Bot Token",
    keychain_account: "slack_bot_token",
    kind: "bearer",
  },
  {
    id: "google_client_id",
    label: "Google OAuth Client ID",
    keychain_account: "google_oauth_client_id",
    kind: "bearer",
  },
  {
    id: "google_client_secret",
    label: "Google OAuth Client Secret",
    keychain_account: "google_oauth_client_secret",
    kind: "bearer",
  },
  {
    id: "google_workspace",
    label: "Google Workspace",
    keychain_account: "google_oauth_refresh_token",
    kind: "oauth",
  },
  {
    id: "x_client_id",
    label: "X (Twitter) Client ID",
    keychain_account: "x_oauth_client_id",
    kind: "bearer",
  },
  {
    id: "x_client_secret",
    label: "X (Twitter) Client Secret",
    keychain_account: "x_oauth_client_secret",
    kind: "bearer",
  },
  {
    id: "x_social",
    label: "X (Twitter)",
    keychain_account: "x_oauth_refresh_token",
    kind: "oauth",
  },
  {
    id: "linkedin_client_id",
    label: "LinkedIn Client ID",
    keychain_account: "linkedin_oauth_client_id",
    kind: "bearer",
  },
  {
    id: "linkedin_client_secret",
    label: "LinkedIn Client Secret",
    keychain_account: "linkedin_oauth_client_secret",
    kind: "bearer",
  },
  {
    id: "linkedin_social",
    label: "LinkedIn",
    keychain_account: "linkedin_oauth_refresh_token",
    kind: "oauth",
  },
  {
    id: "google_ads_client_id",
    label: "Google Ads Client ID",
    keychain_account: "google_ads_client_id",
    kind: "bearer",
  },
  {
    id: "google_ads_client_secret",
    label: "Google Ads Client Secret",
    keychain_account: "google_ads_client_secret",
    kind: "bearer",
  },
  {
    id: "google_ads_developer_token",
    label: "Google Ads Developer Token",
    keychain_account: "google_ads_developer_token",
    kind: "bearer",
  },
  {
    id: "google_ads_customer_id",
    label: "Google Ads Customer ID",
    keychain_account: "google_ads_customer_id",
    kind: "bearer",
  },
  {
    id: "google_ads_login_customer_id",
    label: "Google Ads Login Customer ID (optional)",
    keychain_account: "google_ads_login_customer_id",
    kind: "bearer",
  },
  {
    id: "google_ads",
    label: "Google Ads",
    keychain_account: "google_ads_refresh_token",
    kind: "oauth",
  },
  {
    id: "meta_app_id",
    label: "Meta App ID",
    keychain_account: "meta_app_id",
    kind: "bearer",
  },
  {
    id: "meta_app_secret",
    label: "Meta App Secret",
    keychain_account: "meta_app_secret",
    kind: "bearer",
  },
  {
    id: "meta_ad_account_id",
    label: "Meta Ad Account ID",
    keychain_account: "meta_ad_account_id",
    kind: "bearer",
  },
  {
    id: "meta_ads",
    label: "Meta Ads",
    keychain_account: "meta_long_lived_token",
    kind: "oauth",
  },
  {
    id: "whoop_client_id",
    label: "WHOOP Client ID",
    keychain_account: "whoop_oauth_client_id",
    kind: "bearer",
  },
  {
    id: "whoop_client_secret",
    label: "WHOOP Client Secret",
    keychain_account: "whoop_oauth_client_secret",
    kind: "bearer",
  },
  {
    id: "whoop",
    label: "WHOOP",
    keychain_account: "whoop_oauth_refresh_token",
    kind: "oauth",
  },
  {
    id: "homeassistant_url",
    label: "Home Assistant URL",
    keychain_account: "homeassistant_url",
    kind: "bearer",
  },
  {
    id: "homeassistant_token",
    label: "Home Assistant Token",
    keychain_account: "homeassistant_token",
    kind: "bearer",
  },
  {
    id: "plaid_client_id",
    label: "Plaid Client ID",
    keychain_account: "plaid_client_id",
    kind: "bearer",
  },
  {
    id: "plaid_secret",
    label: "Plaid Secret",
    keychain_account: "plaid_secret",
    kind: "bearer",
  },
  {
    id: "plaid_access_token",
    label: "Plaid Access Token",
    keychain_account: "plaid_access_token",
    kind: "bearer",
  },
  {
    id: "maps_api_key",
    label: "Maps Platform API Key",
    keychain_account: "maps_api_key",
    kind: "bearer",
  },
  {
    id: "hibp_api_key",
    label: "Have I Been Pwned API Key",
    keychain_account: "hibp_api_key",
    kind: "bearer",
  },
  {
    id: "elevenlabs_api_key",
    label: "ElevenLabs API Key (cloud voice)",
    keychain_account: "elevenlabs_api_key",
    kind: "bearer",
  },
] as const;

/** Bearer credentials (paste + verify rows). */
export const BEARER_CREDENTIALS: readonly Credential[] = CREDENTIALS.filter(
  (c) => c.kind === "bearer",
);

/** OAuth credentials (connection-STATUS rows: the secret is the refresh token
 *  the daemon writes after a browser consent — never pasted here). */
export const OAUTH_CREDENTIALS: readonly Credential[] = CREDENTIALS.filter(
  (c) => c.kind === "oauth",
);

/** Look up a credential by id, or null if unknown. */
export function credentialById(id: string): Credential | null {
  return CREDENTIALS.find((c) => c.id === id) ?? null;
}

/** Resolve an id to its Keychain account, or null if the id is unknown. */
export function accountForId(id: string): string | null {
  return credentialById(id)?.keychain_account ?? null;
}

/** Allowlist check mirrored on the backend — is this a known Keychain account? */
export function isKnownAccount(account: string): boolean {
  return CREDENTIALS.some((c) => c.keychain_account === account);
}

/* ------------------------------------------------------------- panel hints */

/**
 * UI-only one-line scope hint shown under a bearer row to tell the user exactly
 * what token shape works. This is presentation copy, NOT part of the credential
 * contract — it lives outside the `Credential` struct so the registry stays
 * lockstep with the Rust allowlist. Keyed by credential id; an id with no entry
 * simply renders no hint.
 */
export const CRED_HINTS: Readonly<Record<string, string>> = {
  github: "Fine-grained or classic PAT with repo scope",
  slack: "Bot token, xoxb-…, with channels:read + chat:write",
  google_client_id:
    "Google Cloud Console → APIs & Services → Credentials → Create OAuth client ID → Desktop app. Ends in .apps.googleusercontent.com",
  google_client_secret:
    "From the same Desktop-app OAuth client. Consent screen must grant: calendar.events, gmail.readonly, gmail.send, drive.file, drive.metadata.readonly",
  x_client_id:
    "X Developer Portal → Projects & Apps → your OAuth 2.0 app → Client ID. The app needs User authentication set up with the tweet.write scope (posting requires a paid API tier).",
  x_client_secret:
    "From the same X OAuth 2.0 app (Client Secret). Stored only — the browser consent runs in darwind via 'connect X'.",
  linkedin_client_id:
    "LinkedIn Developer Portal → your app → Auth → Client ID. Add the posting product (Share on LinkedIn / w_member_social) — it must be approved before posting works.",
  linkedin_client_secret:
    "From the same LinkedIn app (Client Secret). Stored only — the browser consent runs in darwind via 'connect LinkedIn'.",
  google_ads_client_id:
    "Google Cloud Console OAuth client ID (Desktop app), enabled for the Google Ads API (adwords scope). Separate from the Workspace client. Ends in .apps.googleusercontent.com",
  google_ads_client_secret:
    "From the same Google Ads OAuth client (Client Secret). Stored only — the browser consent runs in darwind via 'connect Google Ads'.",
  google_ads_developer_token:
    "Google Ads API Center → Developer token. Sent on every Ads call (developer-token header) — not used for the connect step.",
  google_ads_customer_id:
    "The Google Ads account customer id (digits only, no dashes), e.g. 1234567890. Goes in the resource path on every call.",
  google_ads_login_customer_id:
    "Optional manager (MCC) login customer id, set only when the login account differs from the operating account (login-customer-id header).",
  meta_app_id:
    "Meta for Developers → your app → App ID. The app needs the Marketing API with ads_read + ads_management.",
  meta_app_secret:
    "From the same Meta app (App Secret). Stored only — the browser consent runs in darwind via 'connect Meta'; it exchanges the code for a long-lived token.",
  meta_ad_account_id:
    "The Meta ad account id including the act_ prefix, e.g. act_1234567890.",
  whoop_client_id:
    "WHOOP Developer Dashboard → your app → Client ID. The app needs the read scopes (recovery, cycles, sleep, workout, profile) and offline access. Reads only — Vitalis never writes WHOOP data.",
  whoop_client_secret:
    "From the same WHOOP app (Client Secret). Stored only — the browser consent runs in darwind via 'connect WHOOP'.",
  homeassistant_url:
    "Your Home Assistant base URL, e.g. http://homeassistant.local:8123 or https://your-hub.example.com. DUM-E controls devices through THIS hub — DARWIN does not talk HomeKit directly.",
  homeassistant_token:
    "Home Assistant → your profile → Long-Lived Access Tokens → Create Token. Reads device states and, only when you confirm, calls services to control them.",
  plaid_client_id:
    "Plaid Dashboard → Team Settings → Keys → client_id. From your own Plaid app. MIDAS reads balances and spending only — it never moves money.",
  plaid_secret:
    "Plaid Dashboard → Team Settings → Keys → secret (sandbox or production). From the same Plaid app. Stored only; MIDAS reads — it cannot transfer, pay, or trade.",
  plaid_access_token:
    "A linked-institution access token from Plaid Link (access-…). Plaid Link runs in your own frontend/sandbox — DARWIN does not perform it; paste the resulting token here. Read-only.",
  maps_api_key:
    "Google Maps Platform → Credentials → API key, with Directions, Places, and Distance Matrix enabled. VOYAGER reads routes, places, and travel times only — it never books or pays. The key rides the request header, never a logged URL.",
  hibp_api_key:
    "haveibeenpwned.com → API key (a paid subscription). AEGIS checks your OWN email's breach exposure only — it never scans anyone else, cracks anything, or fetches leaked passwords. The key rides the request header, never a logged URL.",
  elevenlabs_api_key:
    "elevenlabs.io → Profile → API key. OPTIONAL premium CLOUD voices. OFF by default: the cloud voice tier ships disabled, and on-device Kokoro stays the private/offline default + fallback. With the tier on + this key + online, DARWIN's spoken text is sent to ElevenLabs to synthesize (it leaves the device). The key rides the xi-api-key header, never a logged URL. Voice-only — DARWIN keeps its own brain/turn-taking.",
} as const;

/** The scope hint for a credential id, or null when there is none. */
export function hintForId(id: string): string | null {
  return CRED_HINTS[id] ?? null;
}

/* ------------------------------------------------------------ pill mapping */

/**
 * The verification status the backend returns (CONTRACT part B) plus the
 * client-only transient/idle states the pill walks through.
 */
export type VerifyStatus = "valid" | "unauthorized" | "network_error";

/** Pill display state for one credential row. */
export type PillState =
  | { kind: "empty" } // nothing typed, nothing on file
  | { kind: "on_file" } // a verified secret is stored
  | { kind: "verifying" } // Enter pressed, awaiting the backend
  | { kind: "valid"; detail: string } // verified+stored this session
  | { kind: "invalid"; detail: string } // rejected (unauthorized)
  | { kind: "network"; detail: string }; // could not reach the service

/** Typed result of verify_and_store / verify_credential from the backend. */
export interface VerifyResult {
  status: VerifyStatus;
  detail: string;
  /** present on verify_and_store; absent on bare verify_credential. */
  stored?: boolean;
}

/**
 * Map a backend verify result to a pill state (PURE — unit-tested). When the
 * secret verified AND was stored, the row collapses to ON FILE; an
 * unauthorized result is INVALID; a network error is its own amber state.
 */
export function pillFromVerify(result: VerifyResult): PillState {
  switch (result.status) {
    case "valid":
      // verify_and_store sets stored=true on valid; treat a valid-but-unstored
      // result (bare verify) as the transient VALID badge.
      return result.stored
        ? { kind: "on_file" }
        : { kind: "valid", detail: result.detail };
    case "unauthorized":
      return { kind: "invalid", detail: result.detail };
    case "network_error":
      return { kind: "network", detail: result.detail };
  }
}

/** Map a keychain_status presence bool to the resting pill state. */
export function pillFromPresence(onFile: boolean): PillState {
  return onFile ? { kind: "on_file" } : { kind: "empty" };
}

/** FUI token class for a pill state (drives colour: learn-green / alert-red /
 *  warn-amber / dim). */
export function pillClass(state: PillState): string {
  switch (state.kind) {
    case "valid":
    case "on_file":
      return "good";
    case "invalid":
      return "bad";
    case "network":
      return "warn";
    case "verifying":
    case "empty":
      return "idle";
  }
}

/** Short uppercase label for a pill state. */
export function pillLabel(state: PillState): string {
  switch (state.kind) {
    case "empty":
      return "—";
    case "on_file":
      return "ON FILE";
    case "verifying":
      return "VERIFYING…";
    case "valid":
      return "VALID";
    case "invalid":
      return "INVALID";
    case "network":
      return "NETWORK ERROR";
  }
}

/** Strict server-name shape an MCP token account stem must match — mirrors the
 *  daemon's `integrations::is_safe_mcp_server_name` (lowercase alnum + single
 *  `_`/`-` separators, no leading/trailing or CONSECUTIVE separator). The `__`
 *  ban is load-bearing: the flat tool id `mcp__<server>__<tool>` splits on the
 *  first `__`, so a server name containing `__` would mis-route. Kept in lockstep
 *  with the Rust validator. */
const MCP_SERVER_NAME = /^[a-z0-9]+(?:[_-][a-z0-9]+)*$/;

/** The Keychain account for an MCP server's token (`mcp_<server>_token`), or null
 *  when `server` is not the strict allowlisted shape. Computing it here (and
 *  returning null for a bad name) means the settings panel can only ever offer to
 *  store a token under an account the daemon AND the Rust allowlist would accept —
 *  a hostile server name yields no account, so no write is attempted. Pure. */
export function mcpTokenAccount(server: string): string | null {
  return MCP_SERVER_NAME.test(server) ? `mcp_${server}_token` : null;
}
