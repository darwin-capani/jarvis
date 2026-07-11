//! Google Drive client for agents "friday" (Daily Intel), "pepper" (Personal
//! EA) and "veronica" (Content + Comms).
//!
//! Thin, typed wrapper over the shared integration foundation
//! ([`crate::integrations`]) layered on the Google OAuth2 core
//! ([`crate::integrations::google_oauth`]): it is generic over the foundation's
//! [`HttpTransport`] (so production wires [`ReqwestTransport`] and tests wire
//! `MockTransport` — zero network in tests), holds the Drive API transport, and
//! shares the [`GoogleAuth`] handle the three Google service clients use. The
//! access token is NEVER held on this struct — it is fetched per request from
//! [`GoogleAuth::bearer`] at the moment of each send and attached to the
//! outbound `Authorization` header only. The token VALUE is never logged, never
//! put in an error or a Debug field — only its presence (a bool) is recorded.
//! The Drive client never touches the refresh token: `GoogleAuth` owns it.
//!
//! Two tiers of methods, mirroring the foundation's safety model:
//!   * READ (safe — no gate): `list_files`, `search_files`,
//!     `get_file_metadata` — plain GETs against `files.list` / `files/{id}`,
//!     asking only for the `id,name,mimeType,modifiedTime` projection. These need
//!     only the `drive.metadata.readonly` scope (no file CONTENT is read),
//!     matching the least-privilege scope set the OAuth core grants.
//!   * CONSEQUENTIAL (gated): `upload_text_file` — takes an [`ActionMode`]. In
//!     [`ActionMode::DryRun`] it builds and returns a human-readable preview
//!     (name + byte size) and issues NO request; only in [`ActionMode::Execute`]
//!     does it issue EXACTLY ONE multipart upload. Call sites get the mode from
//!     the foundation's `gate(confirm)`, so with `[integrations].allow_consequential`
//!     false (the shipped default) the upload always previews. The content is
//!     PASSED IN (small text) — this client deliberately never reads arbitrary
//!     local files.
//!
//! Every method returns a concise human-facing `String` while parsing the typed
//! fields it needs from the JSON (file id/name/mimeType/modifiedTime). Non-2xx
//! responses map to friendly, secret-free errors via [`map_status`] (401 ->
//! reconnect, 403 -> scope, 404 -> not found).

use std::sync::Arc;

use serde::Deserialize;
use tracing::info;

use super::google_oauth::GoogleAuth;
use super::{
    status_outcome, ActionMode, HttpMethod, HttpRequest, HttpTransport, IntegrationResult,
    StatusOutcome,
};

/// Drive API v3 base for metadata/list/get. All read paths append to this.
const API_BASE: &str = "https://www.googleapis.com/drive/v3";
/// Separate upload host for media uploads (Google routes uploads here).
const UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3/files";
/// The field projection we ask Drive for on a single-file get — exactly the
/// fields we surface, keeping responses (and any logging) tight.
const FILE_FIELDS: &str = "id,name,mimeType,modifiedTime";
/// `files.list` wraps the projection under `files(...)`; the get endpoint takes
/// the bare field list. Factored so the two stay in lockstep.
const LIST_FIELDS: &str = "files(id,name,mimeType,modifiedTime)";
/// A hard ceiling on how many files we ask for, so a runaway `max` can't request
/// an enormous page. Drive's own per-page max is 1000.
const MAX_PAGE: u32 = 1000;
/// A safety ceiling on the SIZE of an uploaded text body (bytes). This client is
/// for small text content (notes, summaries) — not arbitrary file shipping — so
/// we reject anything larger rather than streaming a big payload.
const MAX_UPLOAD_BYTES: usize = 5 * 1024 * 1024;
/// MIME boundary for the multipart/related upload body. A fixed, secret-free
/// literal — the request carries no token in its body, only the bearer header.
const MULTIPART_BOUNDARY: &str = "jarvis-drive-boundary";

// ---------------------------------------------------------------------------
// Typed response shapes — only the fields we surface are decoded. `#[serde(default)]`
// on the soft fields keeps parsing resilient to the many extra keys Drive
// returns and to objects that omit an optional field.
// ---------------------------------------------------------------------------

/// One Drive file's metadata, as returned by `files.list` (inside `files[]`) and
/// by `GET files/{id}` (at the top level). Drive returns camelCase keys
/// (`mimeType`/`modifiedTime`); serde maps them onto the snake_case fields.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
struct DriveFile {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    mime_type: String,
    #[serde(default)]
    modified_time: String,
}

/// The `files.list` envelope: a page of files (other keys like
/// `nextPageToken`/`incompleteSearch` are ignored).
#[derive(Debug, Clone, Deserialize)]
struct FileList {
    #[serde(default)]
    files: Vec<DriveFile>,
}

impl DriveFile {
    /// A short "name (type)" label for a one-line summary.
    fn label(&self) -> String {
        if self.mime_type.is_empty() {
            self.name.clone()
        } else {
            format!("{} ({})", self.name, short_mime(&self.mime_type))
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Google Drive client bound to a transport and the SHARED [`GoogleAuth`] handle
/// the three Google service clients use.
///
/// Construct with [`DriveClient::connect`] (production: resolves the OAuth
/// credentials from the Keychain and wires the real reqwest transport) or, in
/// tests, [`DriveClient::new`] (an explicit `GoogleAuth` over a mock + a
/// `MockTransport` for the Drive calls). The access token is never held on this
/// struct — it is fetched per request from `auth.bearer()` and attached to the
/// outbound `Authorization` header only.
pub struct DriveClient<T: HttpTransport> {
    /// The Drive API transport (production: reqwest; tests: a MockTransport).
    transport: T,
    /// The shared Google auth handle that mints/refreshes access tokens. Held by
    /// `Arc` so Calendar/Gmail/Drive can share ONE handle (one cached token, one
    /// refresh path) rather than each re-resolving the refresh token.
    auth: Arc<GoogleAuth<T>>,
}

/// Custom `Debug` that prints NOTHING about the auth handle's secrets — only that
/// the client is wired. `GoogleAuth`'s own `Debug` already redacts; this keeps a
/// `{:?}` of the Drive client equally safe.
impl<T: HttpTransport> std::fmt::Debug for DriveClient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveClient")
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport> DriveClient<T> {
    /// Build a client over `transport`, sharing the supplied [`GoogleAuth`]
    /// handle. Used by tests (a `GoogleAuth<MockTransport>` paired with a
    /// `MockTransport` for the Drive calls) and by any caller that already holds
    /// the shared handle. No secret is read or stored here.
    pub fn new(transport: T, auth: Arc<GoogleAuth<T>>) -> Self {
        Self { transport, auth }
    }

    /// Compose a Drive request with a FRESH bearer token from the shared auth
    /// handle, attaching it HERE — at the moment of the call — and nowhere else.
    /// The token value is never stored on the client and never logged.
    async fn request(&self, method: HttpMethod, url: String) -> IntegrationResult<HttpRequest> {
        let token = self.auth.bearer().await?;
        Ok(HttpRequest::new(method, url).header("Authorization", format!("Bearer {token}")))
    }

    // -- READ (safe — no gate) -----------------------------------------------

    /// List recent Drive files (up to `max`, newest first by modifiedTime).
    /// Optional `query` is a raw Drive `q` expression for advanced callers; most
    /// callers pass `None`. Read-only — needs only metadata scope. Returns a
    /// one-line summary naming the first few files.
    pub async fn list_files(&self, max: u32, query: Option<&str>) -> IntegrationResult<String> {
        let page_size = max.clamp(1, MAX_PAGE);
        let mut url = format!(
            "{API_BASE}/files?pageSize={page_size}&orderBy={}&fields={}",
            urlencode("modifiedTime desc"),
            urlencode(LIST_FIELDS),
        );
        if let Some(q) = query {
            let q = q.trim();
            if !q.is_empty() {
                url.push_str(&format!("&q={}", urlencode(q)));
            }
        }
        let req = self.request(HttpMethod::Get, url).await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "listing Drive files")?;

        let list: FileList = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("listing Drive files returned an unexpected response"))?;
        info!(count = list.files.len(), "google_drive: listed files");
        Ok(summarize(&list.files, "file"))
    }

    /// Search Drive by file NAME (`name contains '<text>'`), newest first, up to
    /// `max`. Read-only — metadata scope only. Returns a count plus the first few
    /// matches.
    pub async fn search_files(&self, text: &str, max: u32) -> IntegrationResult<String> {
        let page_size = max.clamp(1, MAX_PAGE);
        // Drive's `q` uses single-quoted literals; escape any embedded quote/
        // backslash so a name with an apostrophe can't break (or inject into) the
        // query expression.
        let q = format!("name contains '{}'", escape_drive_query_literal(text));
        let url = format!(
            "{API_BASE}/files?pageSize={page_size}&q={}&orderBy={}&fields={}",
            urlencode(&q),
            urlencode("modifiedTime desc"),
            urlencode(LIST_FIELDS),
        );
        let req = self.request(HttpMethod::Get, url).await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "searching Drive")?;

        let list: FileList = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("searching Drive returned an unexpected response"))?;
        info!(count = list.files.len(), "google_drive: searched files");
        if list.files.is_empty() {
            return Ok(format!("No Drive files matched \"{}\".", text.trim()));
        }
        Ok(summarize(&list.files, "match"))
    }

    /// Fetch one file's metadata by id. Read-only. Returns its name, type, and
    /// last-modified time.
    pub async fn get_file_metadata(&self, file_id: &str) -> IntegrationResult<String> {
        let url = format!(
            "{API_BASE}/files/{}?fields={}",
            encode_path_segment(file_id),
            urlencode(FILE_FIELDS),
        );
        let req = self.request(HttpMethod::Get, url).await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "fetching the Drive file")?;

        let file: DriveFile = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("the Drive file response was not in the expected shape"))?;
        info!(has_id = !file.id.is_empty(), "google_drive: fetched file metadata");
        let modified = if file.modified_time.is_empty() {
            String::new()
        } else {
            format!(", last modified {}", file.modified_time)
        };
        Ok(format!(
            "\"{}\" — {}{} (id {}).",
            file.name,
            short_mime(&file.mime_type),
            modified,
            file.id
        ))
    }

    // -- CONSEQUENTIAL (gated by ActionMode) ---------------------------------

    /// Upload a small TEXT file with the given `name`, `content` and `mime` type.
    ///
    /// In [`ActionMode::DryRun`] this issues NO request — it returns a preview of
    /// exactly what would be uploaded (name + byte size). In [`ActionMode::Execute`]
    /// it issues EXACTLY ONE multipart/related upload (metadata + media in a single
    /// request) and returns the new file's id. Callers obtain `mode` from the
    /// foundation's `gate(confirm)`, so the shipped default (gate OFF) always
    /// previews.
    ///
    /// The content is passed in (small text only — see [`MAX_UPLOAD_BYTES`]); this
    /// client never reads arbitrary local files.
    pub async fn upload_text_file(
        &self,
        name: &str,
        content: &str,
        mime: &str,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        let size = content.len();
        if size > MAX_UPLOAD_BYTES {
            return Err(anyhow::anyhow!(
                "that upload is too large ({size} bytes); this client only handles small text files"
            ));
        }
        let mime = if mime.trim().is_empty() {
            "text/plain"
        } else {
            mime.trim()
        };

        if mode == ActionMode::DryRun {
            info!(size, "google_drive: dry-run upload (no request issued)");
            return Ok(format!(
                "[dry run] Would upload \"{name}\" ({size} bytes, {mime}) to Drive. \
                 Enable consequential actions and confirm to upload."
            ));
        }

        // Multipart/related: a JSON metadata part followed by the media part, in
        // one request to the upload host. `uploadType=multipart` tells Drive to
        // parse both parts from the single body.
        let url = format!(
            "{UPLOAD_BASE}?uploadType=multipart&fields={}",
            urlencode(FILE_FIELDS)
        );
        let req = self
            .request(HttpMethod::Post, url)
            .await?
            .header(
                "Content-Type",
                format!("multipart/related; boundary={MULTIPART_BOUNDARY}"),
            )
            // The assembled multipart text is sent verbatim (not JSON-encoded),
            // with the multipart Content-Type set above.
            .raw_body(multipart_related_body(name, content, mime));
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "uploading the Drive file")?;

        let file: DriveFile = serde_json::from_str(&resp.body).unwrap_or_default();
        info!(has_id = !file.id.is_empty(), size, "google_drive: uploaded file");
        let tail = if file.id.is_empty() {
            String::new()
        } else {
            format!(" (id {})", file.id)
        };
        Ok(format!("Uploaded \"{name}\" to Drive{tail}."))
    }
}

impl DriveClient<super::ReqwestTransport> {
    /// Production constructor: build the shared [`GoogleAuth`] handle from the
    /// Keychain (via `GoogleAuth::connect`) and wire a real reqwest transport for
    /// the Drive calls. Returns the friendly "not connected" error when Google
    /// has not been connected in Settings.
    ///
    /// NOTE: this builds a Drive-OWNED auth handle. When friday/pepper/veronica
    /// share one process-wide handle across Calendar/Gmail/Drive, prefer
    /// [`DriveClient::new`] with the shared `Arc<GoogleAuth>` instead, so all
    /// three reuse one cached access token and one refresh path.
    pub async fn connect() -> IntegrationResult<Self> {
        let auth = GoogleAuth::<super::ReqwestTransport>::connect().await?;
        Ok(Self {
            transport: super::ReqwestTransport::new(),
            auth: Arc::new(auth),
        })
    }
}

// ---------------------------------------------------------------------------
// `Default` for `DriveFile` so a malformed-but-2xx upload body degrades
// gracefully instead of failing the whole call after the file was created.
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// Helpers (pure — unit-testable without a transport)
// ---------------------------------------------------------------------------

/// Map a Drive status to a friendly, secret-free error. 2xx is `Ok`. The task's
/// mappings: 401 -> reconnect (the token expired/was revoked); 403 -> a scope
/// hint (the grant is missing a required Drive scope); 404 -> not found; 429/5xx
/// -> transient. The provider body (which can echo tokens or PII) is never
/// included.
fn map_status(status: u16, what: &str) -> IntegrationResult<()> {
    // 401 and 403 both classify as Unauthorized in the foundation, but the task
    // pins DIFFERENT messages for them, so we branch on the raw code first.
    match status {
        401 => return Err(anyhow::anyhow!("Google access expired — reconnect in Settings")),
        403 => {
            return Err(anyhow::anyhow!(
                "{what} was refused — the Google grant is missing a required Drive scope; reconnect Google in Settings"
            ))
        }
        _ => {}
    }
    match status_outcome(status) {
        StatusOutcome::Success => Ok(()),
        StatusOutcome::NotFound => Err(anyhow::anyhow!(
            "{what} failed — that Drive file was not found"
        )),
        StatusOutcome::RateLimited => Err(anyhow::anyhow!(
            "{what} was rate limited by Google; try again shortly"
        )),
        StatusOutcome::ServerError => Err(anyhow::anyhow!(
            "{what} failed on Google's side; this is usually transient"
        )),
        // Any remaining 4xx / unexpected code leans on the foundation's phrasing.
        other => Err(anyhow::anyhow!("{what} {}", other.friendly())),
    }
}

/// Build the one-line summary shared by `list_files` and `search_files`: a count
/// plus the first few "name (type)" labels, with an "and N more" tail. `noun` is
/// the singular form ("file", "match"); it is pluralized via [`pluralize`].
fn summarize(files: &[DriveFile], noun: &str) -> String {
    if files.is_empty() {
        return format!("No {} found.", pluralize(noun));
    }
    let labels: Vec<String> = files.iter().take(5).map(DriveFile::label).collect();
    let more = files.len().saturating_sub(labels.len());
    let n = files.len();
    let mut out = format!(
        "{n} {}: {}",
        if n == 1 { noun.to_string() } else { pluralize(noun) },
        labels.join(", ")
    );
    if more > 0 {
        out.push_str(&format!(", and {more} more"));
    }
    out.push('.');
    out
}

/// English plural of the few nouns this client uses: words ending in a sibilant
/// (s/x/z/ch/sh) take "es" ("match" -> "matches"); everything else takes "s"
/// ("file" -> "files"). Pure.
fn pluralize(noun: &str) -> String {
    if noun.ends_with('s')
        || noun.ends_with('x')
        || noun.ends_with('z')
        || noun.ends_with("ch")
        || noun.ends_with("sh")
    {
        format!("{noun}es")
    } else {
        format!("{noun}s")
    }
}

/// Shorten a MIME type to its trailing token for a readable label, e.g.
/// `application/vnd.google-apps.document` -> `document`, `text/plain` -> `plain`.
/// Pure. Falls back to "file" for a blank MIME, and to the whole string when
/// there is no separator.
fn short_mime(mime: &str) -> String {
    if mime.is_empty() {
        return "file".to_string();
    }
    mime.rsplit(['.', '/']).next().unwrap_or(mime).to_string()
}

/// Escape a string for embedding inside a single-quoted Drive `q` literal:
/// backslash and single-quote are the only characters that can break/inject the
/// expression, so we backslash-escape both (Drive's documented escaping). Pure.
fn escape_drive_query_literal(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Assemble a multipart/related upload body: a JSON metadata part (the file
/// name) followed by the media part (the text content), bounded by
/// [`MULTIPART_BOUNDARY`]. Pure, so the wire shape is unit-testable. Carries no
/// secret — the bearer rides only in the header.
fn multipart_related_body(name: &str, content: &str, mime: &str) -> String {
    let metadata = serde_json::json!({ "name": name }).to_string();
    format!(
        "--{b}\r\n\
         Content-Type: application/json; charset=UTF-8\r\n\r\n\
         {metadata}\r\n\
         --{b}\r\n\
         Content-Type: {mime}\r\n\r\n\
         {content}\r\n\
         --{b}--\r\n",
        b = MULTIPART_BOUNDARY,
    )
}

/// Percent-encode a single PATH segment (a file id) per RFC 3986: keep the
/// unreserved set literal, percent-encode everything else, so a crafted id can
/// never escape its segment. Same rule as a query value for our inputs.
fn encode_path_segment(value: &str) -> String {
    urlencode(value)
}

/// Percent-encode a string for use in a URL query value, per RFC 3986: keep the
/// unreserved set literal, percent-encode everything else (notably space, `/`,
/// `'`, `(`, `)` which appear in the field projection, orderBy and `q`). Local +
/// pure so this module needs no `url` dependency (matching google_oauth.rs's own
/// encoder).
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: every case drives the foundation's MockTransport with
// hand-written canned Drive JSON (realistic API SHAPE, never fetched). The
// access token is minted from a canned token-endpoint response on a SEPARATE
// auth mock. No network, no real token, no real Google round-trip.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::google_oauth::{GoogleAuth, RefreshTokenStore, TOKEN_ENDPOINT};
    use crate::integrations::testing::MockTransport;

    // Fake credential values that, if leaked, would be unmistakable.
    const FAKE_CLIENT_ID: &str = "111-FAKE.apps.googleusercontent.com";
    const FAKE_CLIENT_SECRET: &str = "GOCSPX-FAKE-SECRET-NEVER-LEAK";
    const FAKE_REFRESH: &str = "1//FAKE-REFRESH-TOKEN-NEVER-LEAK";
    /// The access token the mocked token endpoint mints. If it ever surfaced in a
    /// human-facing string it would be unmistakable.
    const FAKE_ACCESS: &str = "ya29.FAKE-ACCESS-TOKEN-NEVER-LEAK";

    /// A no-op refresh-token store: the Drive tests never trigger a Keychain
    /// write (no `exchange_code`), but `GoogleAuth::new` requires a store.
    fn noop_store() -> RefreshTokenStore {
        std::sync::Arc::new(|_t: &str| Ok(()))
    }

    /// Build a `GoogleAuth<MockTransport>` whose token endpoint returns
    /// `FAKE_ACCESS` on refresh, so `bearer()` mints a token with NO network. The
    /// refresh token is seeded so `bearer()` takes the refresh path.
    fn auth() -> Arc<GoogleAuth<MockTransport>> {
        let refresh_json = format!(
            r#"{{"access_token":"{FAKE_ACCESS}","expires_in":3599,"token_type":"Bearer"}}"#
        );
        let mock = MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 200, refresh_json);
        Arc::new(GoogleAuth::new(
            mock,
            FAKE_CLIENT_ID,
            FAKE_CLIENT_SECRET,
            FAKE_REFRESH,
            noop_store(),
        ))
    }

    /// Build a Drive client over a Drive `MockTransport` and a fresh mocked auth
    /// handle.
    fn client(drive_mock: MockTransport) -> DriveClient<MockTransport> {
        DriveClient::new(drive_mock, auth())
    }

    // -- realistic canned payloads (hand-written from the Drive API shape) -----

    fn files_list_json() -> &'static str {
        r#"{
          "kind": "drive#fileList",
          "incompleteSearch": false,
          "files": [
            {"id": "1AbC", "name": "Q2 plan.txt", "mimeType": "text/plain",
             "modifiedTime": "2026-06-10T12:00:00.000Z"},
            {"id": "2DeF", "name": "Roadmap", "mimeType": "application/vnd.google-apps.document",
             "modifiedTime": "2026-06-09T08:30:00.000Z"}
          ]
        }"#
    }

    fn file_metadata_json() -> &'static str {
        r#"{"id": "1AbC", "name": "Q2 plan.txt", "mimeType": "text/plain",
            "modifiedTime": "2026-06-10T12:00:00.000Z"}"#
    }

    fn upload_ok_json() -> &'static str {
        // Drive's response to a successful multipart upload (with fields=…).
        r#"{"id": "9XyZ", "name": "note.txt", "mimeType": "text/plain",
            "modifiedTime": "2026-06-14T00:00:00.000Z"}"#
    }

    fn empty_list_json() -> &'static str {
        r#"{"files": []}"#
    }

    // -- READ: parsing -------------------------------------------------------

    #[tokio::test]
    async fn list_files_parses_and_summarizes() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/drive/v3/files", 200, files_list_json());
        let out = client(mock).list_files(10, None).await.unwrap();
        assert!(out.contains("2 files"), "got: {out}");
        assert!(out.contains("Q2 plan.txt"));
        assert!(out.contains("Roadmap"));
        // MIME shortened in the label.
        assert!(out.contains("(document)"), "got: {out}");
    }

    #[tokio::test]
    async fn list_files_sends_orderby_and_fields() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/drive/v3/files", 200, files_list_json());
        let c = client(mock);
        c.list_files(3, None).await.unwrap();
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Get);
        assert!(req.url.contains("pageSize=3"), "got: {}", req.url);
        assert!(req.url.contains("orderBy=modifiedTime%20desc"), "got: {}", req.url);
        assert!(req.url.contains("fields="), "got: {}", req.url);
        // Auth attached, value never asserted.
        assert!(req.has_header("authorization"), "auth attached");
        // No body on a read.
        assert!(req.body.is_none(), "read has no body");
    }

    #[tokio::test]
    async fn list_files_passes_optional_query() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/drive/v3/files", 200, empty_list_json());
        let c = client(mock);
        c.list_files(5, Some("mimeType = 'text/plain'")).await.unwrap();
        let req = c.transport.last_request();
        assert!(req.url.contains("&q="), "query appended: {}", req.url);
    }

    #[tokio::test]
    async fn search_files_builds_name_contains_query() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/drive/v3/files", 200, files_list_json());
        let c = client(mock);
        let out = c.search_files("plan", 10).await.unwrap();
        assert!(out.contains("2 matches"), "got: {out}");

        let req = c.transport.last_request();
        // "name contains 'plan'" percent-encoded: spaces -> %20, quote -> %27.
        assert!(req.url.contains("name%20contains%20%27plan%27"), "got: {}", req.url);
    }

    #[tokio::test]
    async fn search_files_escapes_apostrophe_in_term() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/drive/v3/files", 200, empty_list_json());
        let c = client(mock);
        // A term with an apostrophe must be backslash-escaped INSIDE the literal,
        // then percent-encoded — never break out of the quoted literal.
        c.search_files("Bob's notes", 5).await.unwrap();
        let req = c.transport.last_request();
        // The backslash (%5C) precedes the encoded quote (%27).
        assert!(req.url.contains("%5C%27"), "apostrophe must be escaped: {}", req.url);
    }

    #[tokio::test]
    async fn search_files_empty_result_is_friendly() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/drive/v3/files", 200, empty_list_json());
        let out = client(mock).search_files("nothing", 10).await.unwrap();
        assert!(out.contains("No Drive files matched"), "got: {out}");
        assert!(out.contains("nothing"));
    }

    #[tokio::test]
    async fn get_file_metadata_parses_name_type_modified() {
        let mock =
            MockTransport::new().on(HttpMethod::Get, "/drive/v3/files/1AbC", 200, file_metadata_json());
        let out = client(mock).get_file_metadata("1AbC").await.unwrap();
        assert!(out.contains("Q2 plan.txt"), "got: {out}");
        assert!(out.contains("plain"));
        assert!(out.contains("2026-06-10"));
        assert!(out.contains("1AbC"));
    }

    // -- CONSEQUENTIAL: DryRun issues NO request -----------------------------

    #[tokio::test]
    async fn upload_dry_run_issues_no_request_and_previews() {
        let mock = MockTransport::new(); // no canned responses on purpose
        let c = client(mock);
        let out = c
            .upload_text_file("note.txt", "hello world", "text/plain", ActionMode::DryRun)
            .await
            .unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        assert!(out.contains("note.txt"));
        assert!(out.contains("11 bytes"), "byte size in preview: {out}");
        assert_eq!(
            c.transport.requests().len(),
            0,
            "DryRun must not touch the Drive transport"
        );
    }

    // -- CONSEQUENTIAL: Execute issues EXACTLY one upload --------------------

    #[tokio::test]
    async fn upload_execute_posts_exactly_one_upload_with_name_and_mime() {
        let mock =
            MockTransport::new().on(HttpMethod::Post, "/upload/drive/v3/files", 200, upload_ok_json());
        let c = client(mock);
        let out = c
            .upload_text_file("note.txt", "hello world", "text/plain", ActionMode::Execute)
            .await
            .unwrap();
        assert!(out.contains("Uploaded"), "got: {out}");
        assert!(out.contains("note.txt"));
        assert!(out.contains("9XyZ"), "echoes the new file id: {out}");

        // Exactly ONE request on the Drive transport (the token refresh went to
        // the SEPARATE auth mock, so it doesn't count here).
        assert_eq!(c.transport.requests().len(), 1, "exactly one upload issued");
        let up = c.transport.last_request();
        assert_eq!(up.method, HttpMethod::Post);
        assert!(up.url.contains("/upload/drive/v3/files"), "upload host: {}", up.url);
        assert!(up.url.contains("uploadType=multipart"), "got: {}", up.url);
        assert!(up.has_header("authorization"), "auth attached");
        // The multipart raw body carries the name (metadata part) and the content
        // + mime (media part). The Content-Type names the multipart boundary.
        assert!(up.has_header("content-type"), "multipart content-type set");
        let body = up.raw_body.as_ref().expect("raw multipart body recorded");
        assert!(body.contains("note.txt"), "name in metadata part: {body}");
        assert!(body.contains("hello world"), "content in media part");
        assert!(body.contains("text/plain"), "mime in media part");
        assert!(up.body.is_none(), "raw body, not a JSON body");
    }

    #[tokio::test]
    async fn upload_defaults_blank_mime_to_text_plain() {
        let mock =
            MockTransport::new().on(HttpMethod::Post, "/upload/drive/v3/files", 200, upload_ok_json());
        let c = client(mock);
        c.upload_text_file("note.txt", "hi", "  ", ActionMode::Execute)
            .await
            .unwrap();
        let up = c.transport.last_request();
        let body = up.raw_body.as_ref().expect("raw multipart body recorded");
        assert!(body.contains("text/plain"), "blank mime defaults to text/plain: {body}");
    }

    #[tokio::test]
    async fn upload_rejects_oversize_content() {
        let mock = MockTransport::new(); // never reached
        let c = client(mock);
        let big = "x".repeat(MAX_UPLOAD_BYTES + 1);
        let err = c
            .upload_text_file("big.txt", &big, "text/plain", ActionMode::Execute)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too large"), "got: {err}");
        assert_eq!(c.transport.requests().len(), 0, "no request for oversize");
    }

    // -- error mapping -------------------------------------------------------

    #[tokio::test]
    async fn unauthorized_401_maps_to_reconnect() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/drive/v3/files", 401, "{}");
        let err = client(mock).list_files(5, None).await.unwrap_err();
        assert!(err.to_string().contains("reconnect"), "got: {err}");
    }

    #[tokio::test]
    async fn forbidden_403_maps_to_scope_hint() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/drive/v3/files",
            403,
            r#"{"error":{"message":"Insufficient Permission"}}"#,
        );
        let err = client(mock).list_files(5, None).await.unwrap_err();
        assert!(err.to_string().contains("scope"), "got: {err}");
    }

    #[tokio::test]
    async fn not_found_404_maps_friendly() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/drive/v3/files/ghost",
            404,
            r#"{"error":{"message":"Not Found"}}"#,
        );
        let err = client(mock).get_file_metadata("ghost").await.unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[tokio::test]
    async fn server_error_maps_transient() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/drive/v3/files", 503, "upstream down");
        let err = client(mock).list_files(5, None).await.unwrap_err();
        assert!(err.to_string().contains("transient"), "got: {err}");
    }

    // -- "not connected" propagates from the shared auth handle --------------

    #[tokio::test]
    async fn no_refresh_token_is_not_connected() {
        // An auth handle with an EMPTY refresh token can't mint a bearer, so the
        // first read surfaces the OAuth core's friendly not-connected error and
        // NO Drive request is issued.
        let no_token_auth = Arc::new(GoogleAuth::new(
            MockTransport::new(),
            FAKE_CLIENT_ID,
            FAKE_CLIENT_SECRET,
            "",
            noop_store(),
        ));
        let drive_mock = MockTransport::new(); // no canned Drive responses on purpose
        let c = DriveClient::new(drive_mock, no_token_auth);
        let err = c.list_files(5, None).await.unwrap_err();
        assert!(err.to_string().contains("Google isn't connected"), "got: {err}");
        assert_eq!(c.transport.requests().len(), 0, "no Drive request when not connected");
    }

    // -- the TOKEN never leaks ----------------------------------------------

    /// The access token (minted via the canned refresh) and the underlying
    /// secrets must never appear in the client's `Debug` output, in any returned
    /// outcome string, or in any mapped error. We drive a representative slice of
    /// the surface and scan every produced string for them.
    #[tokio::test]
    async fn token_and_secrets_never_appear_in_any_produced_output() {
        // Debug of the client (defers to GoogleAuth's redacting Debug).
        let dbg = format!("{:?}", client(MockTransport::new()));
        for secret in [FAKE_CLIENT_SECRET, FAKE_REFRESH, FAKE_ACCESS] {
            assert!(!dbg.contains(secret), "Debug leaked a secret: {dbg}");
        }

        // Success outcome strings.
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/drive/v3/files", 200, files_list_json())
            .on(HttpMethod::Post, "/upload/drive/v3/files", 200, upload_ok_json());
        let c = client(mock);
        let ok1 = c.list_files(5, None).await.unwrap();
        let ok2 = c
            .upload_text_file("note.txt", "body", "text/plain", ActionMode::Execute)
            .await
            .unwrap();
        for s in [&ok1, &ok2] {
            for secret in [FAKE_CLIENT_SECRET, FAKE_REFRESH, FAKE_ACCESS] {
                assert!(!s.contains(secret), "outcome leaked a secret: {s}");
            }
        }

        // Error outcome string.
        let err_mock = MockTransport::new().on(HttpMethod::Get, "/drive/v3/files", 401, "{}");
        let err = client(err_mock).list_files(5, None).await.unwrap_err().to_string();
        for secret in [FAKE_CLIENT_SECRET, FAKE_REFRESH, FAKE_ACCESS] {
            assert!(!err.contains(secret), "error leaked a secret: {err}");
        }
    }

    // -- pure helpers --------------------------------------------------------

    #[test]
    fn short_mime_takes_trailing_token() {
        assert_eq!(short_mime("text/plain"), "plain");
        assert_eq!(short_mime("application/vnd.google-apps.document"), "document");
        assert_eq!(short_mime("application/pdf"), "pdf");
        assert_eq!(short_mime(""), "file");
        assert_eq!(short_mime("weird"), "weird");
    }

    #[test]
    fn escape_drive_query_literal_escapes_quote_and_backslash() {
        assert_eq!(escape_drive_query_literal("plain"), "plain");
        assert_eq!(escape_drive_query_literal("Bob's"), "Bob\\'s");
        assert_eq!(escape_drive_query_literal("a\\b"), "a\\\\b");
    }

    #[test]
    fn urlencode_keeps_unreserved_encodes_the_rest() {
        assert_eq!(urlencode("aZ09-_.~"), "aZ09-_.~");
        assert_eq!(urlencode("modifiedTime desc"), "modifiedTime%20desc");
        assert_eq!(urlencode("name contains 'x'"), "name%20contains%20%27x%27");
    }

    #[test]
    fn multipart_body_has_both_parts_and_boundary() {
        let body = multipart_related_body("n.txt", "hello", "text/plain");
        assert!(body.contains(&format!("--{MULTIPART_BOUNDARY}")));
        assert!(body.contains("application/json"), "metadata part header");
        assert!(body.contains("\"name\":\"n.txt\""), "name in metadata");
        assert!(body.contains("text/plain"), "media part header");
        assert!(body.contains("hello"), "media content");
        assert!(
            body.trim_end().ends_with(&format!("--{MULTIPART_BOUNDARY}--")),
            "closing boundary"
        );
    }

    #[test]
    fn summarize_counts_and_truncates() {
        let files: Vec<DriveFile> = (0..7)
            .map(|i| DriveFile {
                id: format!("id{i}"),
                name: format!("f{i}"),
                mime_type: "text/plain".to_string(),
                modified_time: String::new(),
            })
            .collect();
        let s = summarize(&files, "file");
        assert!(s.contains("7 files"), "got: {s}");
        assert!(s.contains("and 2 more"), "got: {s}");
        assert!(summarize(&[], "match").contains("No matches found"));
        assert!(summarize(&files[..1], "match").contains("1 match:"), "singular not pluralized");
    }

    #[test]
    fn pluralize_handles_sibilants() {
        assert_eq!(pluralize("file"), "files");
        assert_eq!(pluralize("match"), "matches");
        assert_eq!(pluralize("box"), "boxes");
        assert_eq!(pluralize("dish"), "dishes");
    }

    #[test]
    fn map_status_table() {
        assert!(map_status(200, "x").is_ok());
        assert!(map_status(401, "x").unwrap_err().to_string().contains("reconnect"));
        assert!(map_status(403, "x").unwrap_err().to_string().contains("scope"));
        assert!(map_status(404, "x").unwrap_err().to_string().contains("not found"));
        assert!(map_status(429, "x").unwrap_err().to_string().contains("rate limited"));
        assert!(map_status(500, "x").unwrap_err().to_string().contains("transient"));
    }
}
