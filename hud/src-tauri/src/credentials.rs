//! Credential registry — the single source of truth for the Tauri backend,
//! mirrored byte-for-byte against the TS registry in `src/core/credentials.ts`
//! (CONTRACT part A). Every Keychain account the HUD is allowed to touch lives
//! here; `keychain_set`/`keychain_status`/`keychain_delete` reject any account
//! NOT on this allowlist, so the frontend can never write an arbitrary item.
//!
//! NB: the Anthropic account string is EXACTLY "anthropic_api_key" because
//! darwind reads that item at startup. Do not rename it.

/// "bearer": paste a token + verify over HTTP. "oauth": a connection-STATUS row
/// whose secret (the refresh token) is written by the daemon after a browser
/// consent flow — never pasted, never verifiable by a bare HTTP call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bearer,
    OAuth,
}

/// One registry entry. `id` dispatches verification; `account` is the Keychain
/// account string under service com.darwin.daemon. `label` is part of the
/// contracted shape and mirrors the TS registry; the backend itself never
/// renders it (the panel does), so it is intentionally read-only here.
#[derive(Debug, Clone, Copy)]
pub struct Credential {
    pub id: &'static str,
    #[allow(dead_code)]
    pub label: &'static str,
    pub account: &'static str,
    pub kind: Kind,
}

/// v1 credential set. Keep lockstep with `CREDENTIALS` in
/// src/core/credentials.ts (same ids, accounts, kinds, order).
pub const CREDENTIALS: &[Credential] = &[
    Credential {
        id: "anthropic",
        label: "Anthropic API Key",
        account: "anthropic_api_key",
        kind: Kind::Bearer,
    },
    Credential {
        id: "github",
        label: "GitHub Token (PAT)",
        account: "github_pat",
        kind: Kind::Bearer,
    },
    Credential {
        id: "slack",
        label: "Slack Bot Token",
        account: "slack_bot_token",
        kind: Kind::Bearer,
    },
    Credential {
        id: "google_client_id",
        label: "Google OAuth Client ID",
        account: "google_oauth_client_id",
        kind: Kind::Bearer,
    },
    Credential {
        id: "google_client_secret",
        label: "Google OAuth Client Secret",
        account: "google_oauth_client_secret",
        kind: Kind::Bearer,
    },
    Credential {
        id: "google_workspace",
        label: "Google Workspace",
        account: "google_oauth_refresh_token",
        kind: Kind::OAuth,
    },
    Credential {
        id: "x_client_id",
        label: "X (Twitter) Client ID",
        account: "x_oauth_client_id",
        kind: Kind::Bearer,
    },
    Credential {
        id: "x_client_secret",
        label: "X (Twitter) Client Secret",
        account: "x_oauth_client_secret",
        kind: Kind::Bearer,
    },
    Credential {
        id: "x_social",
        label: "X (Twitter)",
        account: "x_oauth_refresh_token",
        kind: Kind::OAuth,
    },
    Credential {
        id: "linkedin_client_id",
        label: "LinkedIn Client ID",
        account: "linkedin_oauth_client_id",
        kind: Kind::Bearer,
    },
    Credential {
        id: "linkedin_client_secret",
        label: "LinkedIn Client Secret",
        account: "linkedin_oauth_client_secret",
        kind: Kind::Bearer,
    },
    Credential {
        id: "linkedin_social",
        label: "LinkedIn",
        account: "linkedin_oauth_refresh_token",
        kind: Kind::OAuth,
    },
    Credential {
        id: "google_ads_client_id",
        label: "Google Ads Client ID",
        account: "google_ads_client_id",
        kind: Kind::Bearer,
    },
    Credential {
        id: "google_ads_client_secret",
        label: "Google Ads Client Secret",
        account: "google_ads_client_secret",
        kind: Kind::Bearer,
    },
    Credential {
        id: "google_ads_developer_token",
        label: "Google Ads Developer Token",
        account: "google_ads_developer_token",
        kind: Kind::Bearer,
    },
    Credential {
        id: "google_ads_customer_id",
        label: "Google Ads Customer ID",
        account: "google_ads_customer_id",
        kind: Kind::Bearer,
    },
    Credential {
        id: "google_ads_login_customer_id",
        label: "Google Ads Login Customer ID (optional)",
        account: "google_ads_login_customer_id",
        kind: Kind::Bearer,
    },
    Credential {
        id: "google_ads",
        label: "Google Ads",
        account: "google_ads_refresh_token",
        kind: Kind::OAuth,
    },
    Credential {
        id: "meta_app_id",
        label: "Meta App ID",
        account: "meta_app_id",
        kind: Kind::Bearer,
    },
    Credential {
        id: "meta_app_secret",
        label: "Meta App Secret",
        account: "meta_app_secret",
        kind: Kind::Bearer,
    },
    Credential {
        id: "meta_ad_account_id",
        label: "Meta Ad Account ID",
        account: "meta_ad_account_id",
        kind: Kind::Bearer,
    },
    Credential {
        id: "meta_ads",
        label: "Meta Ads",
        account: "meta_long_lived_token",
        kind: Kind::OAuth,
    },
    Credential {
        id: "whoop_client_id",
        label: "WHOOP Client ID",
        account: "whoop_oauth_client_id",
        kind: Kind::Bearer,
    },
    Credential {
        id: "whoop_client_secret",
        label: "WHOOP Client Secret",
        account: "whoop_oauth_client_secret",
        kind: Kind::Bearer,
    },
    Credential {
        id: "whoop",
        label: "WHOOP",
        account: "whoop_oauth_refresh_token",
        kind: Kind::OAuth,
    },
    Credential {
        id: "homeassistant_url",
        label: "Home Assistant URL",
        account: "homeassistant_url",
        kind: Kind::Bearer,
    },
    Credential {
        id: "homeassistant_token",
        label: "Home Assistant Token",
        account: "homeassistant_token",
        kind: Kind::Bearer,
    },
    Credential {
        id: "plaid_client_id",
        label: "Plaid Client ID",
        account: "plaid_client_id",
        kind: Kind::Bearer,
    },
    Credential {
        id: "plaid_secret",
        label: "Plaid Secret",
        account: "plaid_secret",
        kind: Kind::Bearer,
    },
    Credential {
        id: "plaid_access_token",
        label: "Plaid Access Token",
        account: "plaid_access_token",
        kind: Kind::Bearer,
    },
    Credential {
        id: "maps_api_key",
        label: "Maps Platform API Key",
        account: "maps_api_key",
        kind: Kind::Bearer,
    },
    Credential {
        id: "hibp_api_key",
        label: "Have I Been Pwned API Key",
        account: "hibp_api_key",
        kind: Kind::Bearer,
    },
    // ElevenLabs cloud VOICE TIER (an OPTIONAL premium-TTS layer, not an agent).
    // KEY-based, not OAuth: the user pastes their own ElevenLabs API key. Format-
    // checked locally + stored exactly like the other key rows (no HTTP probe —
    // proving it would make a cloud call). The cloud voice tier ships OFF; this key
    // is just stored until the operator turns the tier on. The daemon reads it from
    // the Keychain and passes it to the inference server only in the request body
    // (xi-api-key header); it never rides a logged URL.
    Credential {
        id: "elevenlabs_api_key",
        label: "ElevenLabs API Key (cloud voice)",
        account: "elevenlabs_api_key",
        kind: Kind::Bearer,
    },
];

/// Look up a credential by id.
pub fn credential_by_id(id: &str) -> Option<&'static Credential> {
    CREDENTIALS.iter().find(|c| c.id == id)
}

/// Resolve an id to its Keychain account string (the id->account map).
pub fn account_for_id(id: &str) -> Option<&'static str> {
    credential_by_id(id).map(|c| c.account)
}

/// Is `account` a validated MCP server-token account (`mcp_<server>_token`)?
/// MCP servers are CONFIGURED at runtime, so their token accounts are not in the
/// static [`CREDENTIALS`] registry — but they are still bounded: the `<server>`
/// stem must be the SAME strict `[a-z0-9_-]+` shape the daemon validates in
/// `integrations::mcp_token_account`, so a hostile account string (path
/// traversal, NUL, spaces, an empty name) can NEVER be admitted. Mirrors the
/// daemon's `account_allowed` extension (`integrations::is_safe_mcp_server_name`):
/// a lowercase-alnum stem with single `_`/`-` separators, no leading/trailing or
/// consecutive separator — so the HUD writes only accounts the daemon would read.
/// Pure.
pub fn is_mcp_token_account(account: &str) -> bool {
    let Some(stem) = account
        .strip_prefix("mcp_")
        .and_then(|s| s.strip_suffix("_token"))
    else {
        return false;
    };
    if stem.is_empty() {
        return false;
    }
    let bytes = stem.as_bytes();
    let is_sep = |b: u8| b == b'_' || b == b'-';
    // No leading/trailing separator (matches the daemon's is_safe_mcp_server_name).
    if is_sep(bytes[0]) || is_sep(bytes[bytes.len() - 1]) {
        return false;
    }
    // Charset [a-z0-9_-] AND no two separators in a row (bans `__`,`--`,`_-`,`-_`),
    // so the `__` namespacing boundary in mcp__<server>__<tool> stays unambiguous.
    bytes.iter().enumerate().all(|(i, &b)| {
        let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || is_sep(b);
        let no_double_sep = !(is_sep(b) && i > 0 && is_sep(bytes[i - 1]));
        ok && no_double_sep
    })
}

/// Allowlist check, mirrored on the frontend — is this a known Keychain
/// account? Unknown accounts are rejected before any Keychain write. Admits the
/// static registry PLUS a validated `mcp_<server>_token` account (bounded to the
/// strict server-name shape) so a configured MCP server's token can be stored
/// through the SAME guarded path, never an arbitrary account.
pub fn is_known_account(account: &str) -> bool {
    CREDENTIALS.iter().any(|c| c.account == account) || is_mcp_token_account(account)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_account_is_unchanged_for_the_daemon() {
        // darwind reads this exact item — renaming it would silently break the
        // daemon's key lookup.
        assert_eq!(account_for_id("anthropic"), Some("anthropic_api_key"));
    }

    #[test]
    fn id_to_account_map_is_complete() {
        assert_eq!(account_for_id("github"), Some("github_pat"));
        assert_eq!(account_for_id("slack"), Some("slack_bot_token"));
        // Google Workspace: two pasted BEARER rows (client id/secret) + one
        // connection-STATUS row whose account is the daemon-written refresh token.
        assert_eq!(
            account_for_id("google_client_id"),
            Some("google_oauth_client_id")
        );
        assert_eq!(
            account_for_id("google_client_secret"),
            Some("google_oauth_client_secret")
        );
        assert_eq!(
            account_for_id("google_workspace"),
            Some("google_oauth_refresh_token")
        );
        // X + LinkedIn: two pasted BEARER client rows each, plus a connection-
        // STATUS row whose account is the daemon-written refresh token.
        assert_eq!(account_for_id("x_client_id"), Some("x_oauth_client_id"));
        assert_eq!(
            account_for_id("x_client_secret"),
            Some("x_oauth_client_secret")
        );
        assert_eq!(account_for_id("x_social"), Some("x_oauth_refresh_token"));
        assert_eq!(
            account_for_id("linkedin_client_id"),
            Some("linkedin_oauth_client_id")
        );
        assert_eq!(
            account_for_id("linkedin_client_secret"),
            Some("linkedin_oauth_client_secret")
        );
        assert_eq!(
            account_for_id("linkedin_social"),
            Some("linkedin_oauth_refresh_token")
        );
        // Google Ads: pasted BEARER rows (client id/secret, developer token,
        // customer id, optional login customer id) + a daemon-written connection-
        // STATUS row (the SEPARATE Ads refresh token, distinct from Workspace's).
        assert_eq!(
            account_for_id("google_ads_client_id"),
            Some("google_ads_client_id")
        );
        assert_eq!(
            account_for_id("google_ads_client_secret"),
            Some("google_ads_client_secret")
        );
        assert_eq!(
            account_for_id("google_ads_developer_token"),
            Some("google_ads_developer_token")
        );
        assert_eq!(
            account_for_id("google_ads_customer_id"),
            Some("google_ads_customer_id")
        );
        assert_eq!(
            account_for_id("google_ads_login_customer_id"),
            Some("google_ads_login_customer_id")
        );
        assert_eq!(
            account_for_id("google_ads"),
            Some("google_ads_refresh_token")
        );
        // Meta Ads: pasted BEARER rows (app id/secret, ad account id) + a daemon-
        // written connection-STATUS row whose account is the long-lived token (Meta
        // has no refresh token).
        assert_eq!(account_for_id("meta_app_id"), Some("meta_app_id"));
        assert_eq!(account_for_id("meta_app_secret"), Some("meta_app_secret"));
        assert_eq!(
            account_for_id("meta_ad_account_id"),
            Some("meta_ad_account_id")
        );
        assert_eq!(account_for_id("meta_ads"), Some("meta_long_lived_token"));
        // WHOOP (vitalis, Health & Biometrics): two pasted BEARER client rows + a
        // daemon-written connection-STATUS row whose account is the refresh token.
        assert_eq!(account_for_id("whoop_client_id"), Some("whoop_oauth_client_id"));
        assert_eq!(
            account_for_id("whoop_client_secret"),
            Some("whoop_oauth_client_secret")
        );
        assert_eq!(account_for_id("whoop"), Some("whoop_oauth_refresh_token"));
        // Home Assistant (dume, Home & Environment): two pasted BEARER rows — the
        // hub URL and a long-lived token. Token-based, not OAuth, so no refresh row.
        assert_eq!(account_for_id("homeassistant_url"), Some("homeassistant_url"));
        assert_eq!(account_for_id("homeassistant_token"), Some("homeassistant_token"));
        // Plaid (midas, Personal Treasury): three pasted BEARER rows — client id,
        // secret, and a linked-institution access token. Token-based, not OAuth, so
        // no daemon-written refresh row. MIDAS reads only — none of these moves money.
        assert_eq!(account_for_id("plaid_client_id"), Some("plaid_client_id"));
        assert_eq!(account_for_id("plaid_secret"), Some("plaid_secret"));
        assert_eq!(account_for_id("plaid_access_token"), Some("plaid_access_token"));
        // Maps (voyager, Travel & Logistics): one pasted BEARER row — the Maps
        // Platform API key. Key-based, not OAuth, so no refresh row. VOYAGER reads
        // routes/places/times only — it never books or pays.
        assert_eq!(account_for_id("maps_api_key"), Some("maps_api_key"));
        // Have I Been Pwned (aegis, Defense & Privacy): one pasted BEARER row — the
        // HIBP API key. Key-based, not OAuth, so no refresh row. AEGIS checks the
        // user's OWN email's breach exposure only — defensive, read-only.
        assert_eq!(account_for_id("hibp_api_key"), Some("hibp_api_key"));
        // ElevenLabs cloud voice tier: one pasted BEARER row — the API key. Key-
        // based, not OAuth, so no refresh row. The cloud voice tier ships OFF;
        // on-device Kokoro stays the default + fallback.
        assert_eq!(account_for_id("elevenlabs_api_key"), Some("elevenlabs_api_key"));
        // The old placeholder rows are gone.
        assert_eq!(account_for_id("google_drive"), None);
        assert_eq!(account_for_id("google_calendar"), None);
        assert_eq!(account_for_id("nope"), None);
    }

    #[test]
    fn allowlist_admits_only_registered_accounts() {
        assert!(is_known_account("anthropic_api_key"));
        assert!(is_known_account("github_pat"));
        assert!(is_known_account("slack_bot_token"));
        assert!(is_known_account("google_oauth_client_id"));
        assert!(is_known_account("google_oauth_client_secret"));
        assert!(is_known_account("google_oauth_refresh_token"));
        assert!(is_known_account("x_oauth_client_id"));
        assert!(is_known_account("x_oauth_client_secret"));
        assert!(is_known_account("x_oauth_refresh_token"));
        assert!(is_known_account("linkedin_oauth_client_id"));
        assert!(is_known_account("linkedin_oauth_client_secret"));
        assert!(is_known_account("linkedin_oauth_refresh_token"));
        // Google Ads + Meta Ads accounts (round 3b).
        assert!(is_known_account("google_ads_client_id"));
        assert!(is_known_account("google_ads_client_secret"));
        assert!(is_known_account("google_ads_developer_token"));
        assert!(is_known_account("google_ads_customer_id"));
        assert!(is_known_account("google_ads_login_customer_id"));
        assert!(is_known_account("google_ads_refresh_token"));
        assert!(is_known_account("meta_app_id"));
        assert!(is_known_account("meta_app_secret"));
        assert!(is_known_account("meta_ad_account_id"));
        assert!(is_known_account("meta_long_lived_token"));
        // WHOOP accounts.
        assert!(is_known_account("whoop_oauth_client_id"));
        assert!(is_known_account("whoop_oauth_client_secret"));
        assert!(is_known_account("whoop_oauth_refresh_token"));
        // Home Assistant accounts (dume).
        assert!(is_known_account("homeassistant_url"));
        assert!(is_known_account("homeassistant_token"));
        // Plaid accounts (midas).
        assert!(is_known_account("plaid_client_id"));
        assert!(is_known_account("plaid_secret"));
        assert!(is_known_account("plaid_access_token"));
        // Maps account (voyager).
        assert!(is_known_account("maps_api_key"));
        // HIBP account (aegis).
        assert!(is_known_account("hibp_api_key"));
        // ElevenLabs cloud voice tier account.
        assert!(is_known_account("elevenlabs_api_key"));
        // The retired placeholder accounts are no longer admitted.
        assert!(!is_known_account("google_drive_oauth"));
        assert!(!is_known_account("google_calendar_oauth"));
        // Anything not in the registry is rejected.
        assert!(!is_known_account("anthropic_api_key\0evil"));
        assert!(!is_known_account("../../etc/passwd"));
        assert!(!is_known_account(""));
        assert!(!is_known_account("login_keychain"));
    }

    #[test]
    fn mcp_token_accounts_are_admitted_only_for_safe_server_names() {
        // Valid server names -> admitted through the SAME guarded path.
        assert!(is_mcp_token_account("mcp_files_token"));
        assert!(is_mcp_token_account("mcp_weather-api_token"));
        assert!(is_known_account("mcp_files_token"), "is_known_account admits MCP tokens");
        // Hostile / malformed -> never admitted (matches the daemon's validation).
        assert!(!is_mcp_token_account("mcp__token"), "empty server stem");
        assert!(!is_mcp_token_account("mcp_../../etc_token"), "path traversal");
        assert!(!is_mcp_token_account("mcp_a b_token"), "space in name");
        assert!(!is_mcp_token_account("mcp_Files_token"), "uppercase rejected");
        assert!(!is_mcp_token_account("mcp_files"), "missing _token suffix");
        assert!(!is_mcp_token_account("files_token"), "missing mcp_ prefix");
        assert!(!is_mcp_token_account("mcp_files_token\0evil"), "NUL byte");
        assert!(!is_mcp_token_account("mcp_a__b_token"), "consecutive separators rejected");
        assert!(!is_mcp_token_account("mcp_-a_token"), "leading separator rejected");
        assert!(!is_mcp_token_account("mcp_a-_token"), "trailing separator rejected");
        assert!(!is_known_account("mcp_../../etc_token"), "guard rejects traversal account");
        assert!(!is_known_account("mcp_a__b_token"), "guard rejects double-separator account");
    }

    #[test]
    fn ids_and_accounts_are_unique() {
        for (i, a) in CREDENTIALS.iter().enumerate() {
            for b in &CREDENTIALS[i + 1..] {
                assert_ne!(a.id, b.id, "duplicate id {}", a.id);
                assert_ne!(a.account, b.account, "duplicate account {}", a.account);
            }
        }
    }

    #[test]
    fn kinds_match_the_contract() {
        assert_eq!(credential_by_id("anthropic").unwrap().kind, Kind::Bearer);
        assert_eq!(credential_by_id("github").unwrap().kind, Kind::Bearer);
        assert_eq!(credential_by_id("slack").unwrap().kind, Kind::Bearer);
        // Client id/secret are pasted (Bearer); the workspace row is the
        // connection STATUS (OAuth — its secret is daemon-written, not pasted).
        assert_eq!(
            credential_by_id("google_client_id").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(
            credential_by_id("google_client_secret").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(
            credential_by_id("google_workspace").unwrap().kind,
            Kind::OAuth
        );
        // X + LinkedIn mirror the Google shape: pasted client id/secret (Bearer)
        // plus a daemon-written connection STATUS row (OAuth).
        assert_eq!(credential_by_id("x_client_id").unwrap().kind, Kind::Bearer);
        assert_eq!(
            credential_by_id("x_client_secret").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(credential_by_id("x_social").unwrap().kind, Kind::OAuth);
        assert_eq!(
            credential_by_id("linkedin_client_id").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(
            credential_by_id("linkedin_client_secret").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(
            credential_by_id("linkedin_social").unwrap().kind,
            Kind::OAuth
        );
        // Google Ads + Meta Ads mirror the same shape: pasted Bearer rows for the
        // values the user supplies, plus a daemon-written connection STATUS row
        // (OAuth) for the refresh / long-lived token.
        assert_eq!(
            credential_by_id("google_ads_client_id").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(
            credential_by_id("google_ads_client_secret").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(
            credential_by_id("google_ads_developer_token").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(
            credential_by_id("google_ads_customer_id").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(
            credential_by_id("google_ads_login_customer_id").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(credential_by_id("google_ads").unwrap().kind, Kind::OAuth);
        assert_eq!(credential_by_id("meta_app_id").unwrap().kind, Kind::Bearer);
        assert_eq!(
            credential_by_id("meta_app_secret").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(
            credential_by_id("meta_ad_account_id").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(credential_by_id("meta_ads").unwrap().kind, Kind::OAuth);
        // WHOOP mirrors the same shape: pasted Bearer client id/secret + a
        // daemon-written connection STATUS row (OAuth) for the refresh token.
        assert_eq!(
            credential_by_id("whoop_client_id").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(
            credential_by_id("whoop_client_secret").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(credential_by_id("whoop").unwrap().kind, Kind::OAuth);
        // Home Assistant: both rows are pasted BEARER values (hub URL + long-lived
        // token). Token-based, not OAuth — there is no daemon-written refresh row.
        assert_eq!(
            credential_by_id("homeassistant_url").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(
            credential_by_id("homeassistant_token").unwrap().kind,
            Kind::Bearer
        );
        // Plaid (midas): three pasted BEARER values (client id, secret, access
        // token). Token-based, not OAuth — there is no daemon-written refresh row.
        assert_eq!(
            credential_by_id("plaid_client_id").unwrap().kind,
            Kind::Bearer
        );
        assert_eq!(credential_by_id("plaid_secret").unwrap().kind, Kind::Bearer);
        assert_eq!(
            credential_by_id("plaid_access_token").unwrap().kind,
            Kind::Bearer
        );
        // Maps (voyager): one pasted BEARER value (the Maps Platform API key).
        // Key-based, not OAuth — there is no refresh row.
        assert_eq!(credential_by_id("maps_api_key").unwrap().kind, Kind::Bearer);
        // HIBP (aegis): one pasted BEARER value (the Have I Been Pwned API key).
        // Key-based, not OAuth — there is no refresh row.
        assert_eq!(credential_by_id("hibp_api_key").unwrap().kind, Kind::Bearer);
        // ElevenLabs cloud voice tier: one pasted BEARER value (the API key).
        // Key-based, not OAuth — there is no refresh row.
        assert_eq!(
            credential_by_id("elevenlabs_api_key").unwrap().kind,
            Kind::Bearer
        );
    }

    #[test]
    fn registry_order_matches_the_ts_mirror() {
        // Lockstep render order with src/core/credentials.ts CREDENTIALS.
        let ids: Vec<&str> = CREDENTIALS.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec![
                "anthropic",
                "github",
                "slack",
                "google_client_id",
                "google_client_secret",
                "google_workspace",
                "x_client_id",
                "x_client_secret",
                "x_social",
                "linkedin_client_id",
                "linkedin_client_secret",
                "linkedin_social",
                "google_ads_client_id",
                "google_ads_client_secret",
                "google_ads_developer_token",
                "google_ads_customer_id",
                "google_ads_login_customer_id",
                "google_ads",
                "meta_app_id",
                "meta_app_secret",
                "meta_ad_account_id",
                "meta_ads",
                "whoop_client_id",
                "whoop_client_secret",
                "whoop",
                "homeassistant_url",
                "homeassistant_token",
                "plaid_client_id",
                "plaid_secret",
                "plaid_access_token",
                "maps_api_key",
                "hibp_api_key",
                "elevenlabs_api_key",
            ]
        );
    }
}
