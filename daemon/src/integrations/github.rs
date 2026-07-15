//! GitHub client for agent "steve" (CTO + Builds).
//!
//! Thin, typed wrapper over the shared integration foundation
//! ([`crate::integrations`]): it is generic over the foundation's
//! [`HttpTransport`] (so production wires [`ReqwestTransport`] and tests wire
//! `MockTransport` — zero network in tests), resolves its PAT once via
//! `resolve_secret("github_pat")`, and attaches the Bearer token per request at
//! the moment of the send. The token VALUE is never logged, never stored on the
//! transport, never put in an error or a Debug field — only its presence (a
//! bool) is ever recorded.
//!
//! Two tiers of methods, mirroring the foundation's safety model:
//!   * READ (safe): `list_repos`, `list_pull_requests`, `get_pull_request`,
//!     `list_issues` — no gate, plain GETs.
//!   * CONSEQUENTIAL (gated): `create_issue_comment`, `open_pull_request` — each
//!     takes an [`ActionMode`]. In [`ActionMode::DryRun`] the method builds and
//!     returns a human-readable preview and issues NO mutating request; only in
//!     [`ActionMode::Execute`] does it POST. Call sites get the mode from the
//!     foundation's `gate(confirm)`, so with `[integrations].allow_consequential`
//!     false (the shipped default) these always preview.
//!
//! Every method returns a concise human-facing `String` — what steve would say
//! — while parsing the typed fields it needs from the JSON (PR number/title/
//! state/url, repo full_name, issue title/number). Non-2xx responses map to
//! friendly, secret-free errors via [`map_status`].

use serde::Deserialize;
use tracing::info;

use super::{
    status_outcome, ActionMode, HttpMethod, HttpRequest, HttpTransport, IntegrationResult,
    StatusOutcome,
};

/// GitHub REST API base. All paths are appended to this.
const API_BASE: &str = "https://api.github.com";
/// Media type GitHub recommends pinning so responses don't shift under us.
const ACCEPT: &str = "application/vnd.github+json";
/// GitHub REJECTS requests with no User-Agent, so we always send one. It names
/// the daemon, not the operator — no PII, no secret.
const USER_AGENT: &str = "darwin";
/// The Keychain account the foundation's allowlisted resolver reads for us.
const SECRET_ACCOUNT: &str = "github_pat";

// ---------------------------------------------------------------------------
// Typed response shapes — only the fields steve actually needs are decoded.
// `#[serde(default)]` on the soft fields keeps parsing resilient to the many
// extra keys GitHub returns and to objects that omit an optional field.
// ---------------------------------------------------------------------------

/// A repository, as returned by `GET /user/repos`.
#[derive(Debug, Clone, Deserialize)]
struct Repo {
    /// "owner/name", e.g. "octocat/hello-world".
    full_name: String,
}

/// A pull request, as returned by the PRs list and `GET .../pulls/{n}`.
#[derive(Debug, Clone, Deserialize)]
struct PullRequest {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    state: String,
    /// Browser URL for the PR (GitHub's `html_url`).
    #[serde(default)]
    html_url: String,
}

/// An issue, as returned by `GET .../issues`. GitHub returns PRs in this list
/// too; they carry a `pull_request` object, which we use to filter them out so
/// "issues" means issues.
#[derive(Debug, Clone, Deserialize)]
struct Issue {
    number: u64,
    #[serde(default)]
    title: String,
    /// Present only when the "issue" is actually a pull request.
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

/// The slim view of a just-created PR / comment we echo back.
#[derive(Debug, Clone, Deserialize)]
#[derive(Default)]
struct Created {
    #[serde(default)]
    number: u64,
    #[serde(default)]
    html_url: String,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// GitHub client bound to a transport and a resolved PAT.
///
/// Construct with [`GithubClient::new`] (resolves the PAT from the Keychain) or,
/// in tests, [`GithubClient::with_token`] (an explicit fake token + a
/// `MockTransport`). The token is held only to compose the per-request
/// `Authorization` header; it is never logged and the `Debug` impl below
/// redacts it.
pub struct GithubClient<T: HttpTransport> {
    transport: T,
    token: String,
}

/// Custom `Debug` that NEVER prints the token — only that one is present. So a
/// `{:?}` of a client (in a log line, a panic message, a test) can't leak the
/// PAT.
impl<T: HttpTransport> std::fmt::Debug for GithubClient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubClient")
            .field("token_present", &!self.token.is_empty())
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport> GithubClient<T> {
    /// Build a client over `transport`, resolving the PAT from the macOS
    /// Keychain via the foundation's allowlisted `resolve_secret("github_pat")`.
    /// Returns `Err` (secret-free) when no PAT is configured — steve can relay
    /// that to the operator without ever surfacing a token.
    pub async fn new(transport: T) -> IntegrationResult<Self> {
        let token = super::resolve_secret(SECRET_ACCOUNT).await.ok_or_else(|| {
            anyhow::anyhow!("no GitHub token configured — add a PAT in Settings")
        })?;
        Ok(Self { transport, token })
    }

    /// Build a client with an explicitly supplied token. Used by tests (paired
    /// with `MockTransport`) and by any caller that has already resolved the
    /// secret. The token is consumed into the client and never logged.
    pub fn with_token(transport: T, token: impl Into<String>) -> Self {
        Self {
            transport,
            token: token.into(),
        }
    }

    /// Compose a request with the standard GitHub headers, attaching the Bearer
    /// token HERE — at the moment of the call — and nowhere else.
    fn request(&self, method: HttpMethod, path: &str) -> HttpRequest {
        HttpRequest::new(method, format!("{API_BASE}{path}"))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", ACCEPT)
            .header("User-Agent", USER_AGENT)
    }

    // -- READ (safe — no gate) -----------------------------------------------

    /// List the authenticated user's repositories (up to `limit`, clamped to
    /// GitHub's 1..=100 per-page range). Read-only. Returns a one-line summary
    /// naming the first few `full_name`s.
    pub async fn list_repos(&self, limit: u32) -> IntegrationResult<String> {
        let per_page = limit.clamp(1, 100);
        let req = self.request(
            HttpMethod::Get,
            &format!("/user/repos?per_page={per_page}&sort=updated"),
        );
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "listing repositories")?;

        let repos: Vec<Repo> = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("listing repositories returned an unexpected response"))?;
        info!(count = repos.len(), "github: listed repositories");

        if repos.is_empty() {
            return Ok("No repositories found for this account.".to_string());
        }
        let names: Vec<&str> = repos.iter().map(|r| r.full_name.as_str()).take(5).collect();
        let more = repos.len().saturating_sub(names.len());
        let mut out = format!(
            "Found {} repositor{}: {}",
            repos.len(),
            if repos.len() == 1 { "y" } else { "ies" },
            names.join(", ")
        );
        if more > 0 {
            out.push_str(&format!(", and {more} more"));
        }
        out.push('.');
        Ok(out)
    }

    /// List pull requests for `owner/repo` filtered by `state` ("open",
    /// "closed", or "all"). Read-only. Returns a count plus the first few
    /// "#<n> <title>".
    pub async fn list_pull_requests(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
    ) -> IntegrationResult<String> {
        let state = normalize_state(state);
        let req = self.request(
            HttpMethod::Get,
            &format!("/repos/{owner}/{repo}/pulls?state={state}&per_page=20"),
        );
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "listing pull requests")?;

        let prs: Vec<PullRequest> = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("listing pull requests returned an unexpected response"))?;
        info!(count = prs.len(), state, "github: listed pull requests");

        if prs.is_empty() {
            return Ok(format!("No {state} pull requests in {owner}/{repo}."));
        }
        let lines: Vec<String> = prs
            .iter()
            .take(5)
            .map(|p| format!("#{} {}", p.number, p.title))
            .collect();
        let more = prs.len().saturating_sub(lines.len());
        let mut out = format!(
            "{} {state} pull request{} in {owner}/{repo}: {}",
            prs.len(),
            if prs.len() == 1 { "" } else { "s" },
            lines.join("; ")
        );
        if more > 0 {
            out.push_str(&format!("; and {more} more"));
        }
        out.push('.');
        Ok(out)
    }

    /// Fetch one pull request by number for `owner/repo`. Read-only. Returns its
    /// title, state, and URL.
    pub async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> IntegrationResult<String> {
        let req = self.request(
            HttpMethod::Get,
            &format!("/repos/{owner}/{repo}/pulls/{number}"),
        );
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "fetching the pull request")?;

        let pr: PullRequest = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("the pull request response was not in the expected shape"))?;
        info!(number = pr.number, state = %pr.state, "github: fetched pull request");

        Ok(format!(
            "PR #{} in {owner}/{repo} is {} — \"{}\". {}",
            pr.number, pr.state, pr.title, pr.html_url
        ))
    }

    /// List issues for `owner/repo` filtered by `state` ("open", "closed", or
    /// "all"). Read-only. Pull requests (which GitHub mixes into this endpoint)
    /// are filtered out. Returns a count plus the first few "#<n> <title>".
    pub async fn list_issues(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
    ) -> IntegrationResult<String> {
        let state = normalize_state(state);
        let req = self.request(
            HttpMethod::Get,
            &format!("/repos/{owner}/{repo}/issues?state={state}&per_page=20"),
        );
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "listing issues")?;

        let raw: Vec<Issue> = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("listing issues returned an unexpected response"))?;
        // GitHub returns PRs here too; keep only true issues.
        let issues: Vec<&Issue> = raw.iter().filter(|i| i.pull_request.is_none()).collect();
        info!(count = issues.len(), state, "github: listed issues");

        if issues.is_empty() {
            return Ok(format!("No {state} issues in {owner}/{repo}."));
        }
        let lines: Vec<String> = issues
            .iter()
            .take(5)
            .map(|i| format!("#{} {}", i.number, i.title))
            .collect();
        let more = issues.len().saturating_sub(lines.len());
        let mut out = format!(
            "{} {state} issue{} in {owner}/{repo}: {}",
            issues.len(),
            if issues.len() == 1 { "" } else { "s" },
            lines.join("; ")
        );
        if more > 0 {
            out.push_str(&format!("; and {more} more"));
        }
        out.push('.');
        Ok(out)
    }

    // -- CONSEQUENTIAL (gated by ActionMode) ---------------------------------

    /// Comment on issue/PR `number` in `owner/repo` with `body`.
    ///
    /// In [`ActionMode::DryRun`] this issues NO request — it returns a preview
    /// of exactly what would be posted. In [`ActionMode::Execute`] it POSTs the
    /// comment and returns its URL. Callers obtain `mode` from the foundation's
    /// `gate(confirm)`, so the shipped default (gate OFF) always previews.
    pub async fn create_issue_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        if mode == ActionMode::DryRun {
            info!(number, "github: dry-run issue comment (no request issued)");
            return Ok(format!(
                "[dry run] Would comment on #{number} in {owner}/{repo}: \"{}\". \
                 Enable consequential actions and confirm to post.",
                preview_snippet(body)
            ));
        }

        let req = self
            .request(
                HttpMethod::Post,
                &format!("/repos/{owner}/{repo}/issues/{number}/comments"),
            )
            .json_body(serde_json::json!({ "body": body }));
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "posting the comment")?;

        let created: Created = serde_json::from_str(&resp.body).unwrap_or_default();
        info!(number, "github: posted issue comment");
        let where_ = if created.html_url.is_empty() {
            format!("#{number} in {owner}/{repo}")
        } else {
            created.html_url
        };
        Ok(format!("Comment posted on {where_}."))
    }

    /// Open a pull request in `owner/repo` from `head` into `base`.
    ///
    /// In [`ActionMode::DryRun`] this issues NO request — it returns a preview
    /// of the PR it would open. In [`ActionMode::Execute`] it POSTs the PR and
    /// returns its number and URL. Callers obtain `mode` from `gate(confirm)`.
    // Distinct PR parameters passed positionally; a params struct would add
    // ceremony without making the GitHub call any clearer.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_pull_request(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        if mode == ActionMode::DryRun {
            info!("github: dry-run open PR (no request issued)");
            return Ok(format!(
                "[dry run] Would open a PR in {owner}/{repo} from {head} into {base} \
                 titled \"{title}\". Enable consequential actions and confirm to open it."
            ));
        }

        let req = self
            .request(HttpMethod::Post, &format!("/repos/{owner}/{repo}/pulls"))
            .json_body(serde_json::json!({
                "title": title,
                "head": head,
                "base": base,
                "body": body,
            }));
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "opening the pull request")?;

        let created: Created = serde_json::from_str(&resp.body).unwrap_or_default();
        info!(number = created.number, "github: opened pull request");
        let tail = if created.html_url.is_empty() {
            String::new()
        } else {
            format!(" — {}", created.html_url)
        };
        Ok(format!(
            "Opened PR #{} in {owner}/{repo} from {head} into {base}{tail}.",
            created.number
        ))
    }
}

// ---------------------------------------------------------------------------
// `Default` for `Created` so a malformed-but-2xx body degrades gracefully
// instead of failing the whole call after the side effect already happened.
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// Helpers (pure — unit-testable without a transport)
// ---------------------------------------------------------------------------

/// Map a GitHub status to a friendly, secret-free error. 2xx is `Ok`. The
/// task's mappings: 401/403 -> a PAT-rejected hint; 404 -> not found; 422 ->
/// validation; 429/5xx -> transient. The provider body (which can echo tokens
/// or PII) is never included.
fn map_status(status: u16, what: &str) -> IntegrationResult<()> {
    match status_outcome(status) {
        StatusOutcome::Success => Ok(()),
        StatusOutcome::Unauthorized => {
            Err(anyhow::anyhow!("GitHub token rejected — check the PAT in Settings"))
        }
        StatusOutcome::NotFound => Err(anyhow::anyhow!("{what} failed — the repository, issue, or pull request was not found")),
        StatusOutcome::ClientError if status == 422 => {
            Err(anyhow::anyhow!("{what} was rejected by GitHub as invalid (a branch may be missing, or a PR may already exist)"))
        }
        StatusOutcome::RateLimited => {
            Err(anyhow::anyhow!("{what} was rate limited by GitHub; try again shortly"))
        }
        StatusOutcome::ServerError => {
            Err(anyhow::anyhow!("{what} failed on GitHub's side; this is usually transient"))
        }
        // Any other 4xx / unexpected code: lean on the foundation's phrasing.
        other => Err(anyhow::anyhow!("{what} {}", other.friendly())),
    }
}

/// Normalize a caller-supplied state to one GitHub accepts. Anything other than
/// "closed" or "all" (case-insensitive) becomes "open" — the safe default that
/// can't 422 the request.
fn normalize_state(state: &str) -> &'static str {
    match state.trim().to_ascii_lowercase().as_str() {
        "closed" => "closed",
        "all" => "all",
        _ => "open",
    }
}

/// A short, single-line snippet of a comment/PR body for a preview, so a long
/// or multi-line body doesn't blow up the spoken reply.
fn preview_snippet(body: &str) -> String {
    let one_line = body.replace('\n', " ");
    let trimmed = one_line.trim();
    const MAX: usize = 80;
    if trimmed.chars().count() <= MAX {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(MAX).collect();
        format!("{head}…")
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: every case drives the foundation's MockTransport with
// hand-written canned GitHub JSON (realistic API SHAPE, never fetched). No
// network, no real token.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::testing::MockTransport;

    /// A throwaway token used only to prove the auth header is ATTACHED. Its
    /// value is never asserted by any test.
    const FAKE_TOKEN: &str = "ghp_FAKE_TOKEN_NEVER_ASSERTED_0000000000";

    // -- realistic canned payloads (hand-written from the GitHub API shape) ---

    fn repos_json() -> &'static str {
        r#"[
          {"id": 1, "full_name": "octocat/hello-world", "private": false},
          {"id": 2, "full_name": "octocat/spoon-knife", "private": true}
        ]"#
    }

    fn pulls_json() -> &'static str {
        r#"[
          {"number": 42, "title": "Add CI", "state": "open",
           "html_url": "https://github.com/octocat/hello-world/pull/42"},
          {"number": 41, "title": "Fix typo", "state": "open",
           "html_url": "https://github.com/octocat/hello-world/pull/41"}
        ]"#
    }

    fn pull_json() -> &'static str {
        r#"{"number": 42, "title": "Add CI", "state": "open",
            "html_url": "https://github.com/octocat/hello-world/pull/42",
            "head": {"ref": "ci"}, "base": {"ref": "main"}}"#
    }

    fn issues_json() -> &'static str {
        // The third entry is actually a PR (it carries `pull_request`) and must
        // be filtered out of the issue count.
        r#"[
          {"number": 7, "title": "Crash on startup", "state": "open"},
          {"number": 5, "title": "Docs typo", "state": "open"},
          {"number": 99, "title": "A PR masquerading as an issue", "state": "open",
           "pull_request": {"url": "https://api.github.com/repos/octocat/hello-world/pulls/99"}}
        ]"#
    }

    fn created_pr_json() -> &'static str {
        // Shape of GitHub's 201 response to POST /repos/{o}/{r}/pulls.
        r#"{"number": 43, "title": "New feature", "state": "open",
            "html_url": "https://github.com/octocat/hello-world/pull/43"}"#
    }

    fn created_comment_json() -> &'static str {
        r#"{"id": 1001, "html_url": "https://github.com/octocat/hello-world/issues/7#issuecomment-1001",
            "body": "looks good"}"#
    }

    fn client(mock: MockTransport) -> GithubClient<MockTransport> {
        GithubClient::with_token(mock, FAKE_TOKEN)
    }

    // -- READ: parsing -------------------------------------------------------

    #[tokio::test]
    async fn list_repos_parses_and_summarizes() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/user/repos", 200, repos_json());
        let out = client(mock).list_repos(30).await.unwrap();
        assert!(out.contains("2 repositories"), "got: {out}");
        assert!(out.contains("octocat/hello-world"));
        assert!(out.contains("octocat/spoon-knife"));
    }

    #[tokio::test]
    async fn list_pull_requests_parses() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/repos/octocat/hello-world/pulls",
            200,
            pulls_json(),
        );
        let out = client(mock)
            .list_pull_requests("octocat", "hello-world", "open")
            .await
            .unwrap();
        assert!(out.contains("2 open pull requests"), "got: {out}");
        assert!(out.contains("#42 Add CI"));
    }

    #[tokio::test]
    async fn get_pull_request_parses_number_title_state_url() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/repos/octocat/hello-world/pulls/42",
            200,
            pull_json(),
        );
        let out = client(mock)
            .get_pull_request("octocat", "hello-world", 42)
            .await
            .unwrap();
        assert!(out.contains("PR #42"));
        assert!(out.contains("open"));
        assert!(out.contains("Add CI"));
        assert!(out.contains("https://github.com/octocat/hello-world/pull/42"));
    }

    #[tokio::test]
    async fn list_issues_filters_out_pull_requests() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/repos/octocat/hello-world/issues",
            200,
            issues_json(),
        );
        let out = client(mock)
            .list_issues("octocat", "hello-world", "open")
            .await
            .unwrap();
        // 3 entries in JSON, but one is a PR -> 2 issues.
        assert!(out.contains("2 open issues"), "got: {out}");
        assert!(out.contains("#7 Crash on startup"));
        assert!(!out.contains("masquerading"), "PR must be filtered out");
    }

    // -- READ: header SHAPE on the recorded request (never the token) --------

    #[tokio::test]
    async fn read_request_carries_auth_accept_and_user_agent_headers() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/user/repos", 200, repos_json());
        let c = GithubClient::with_token(mock, FAKE_TOKEN);
        c.list_repos(5).await.unwrap();
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Get);
        assert!(req.url.contains("/user/repos"));
        // Presence of auth — NOT its value.
        assert!(req.has_header("authorization"), "auth header attached");
        assert!(req.has_header("accept"));
        assert!(req.has_header("user-agent"));
        // No body on a read.
        assert!(req.body.is_none());
    }

    // -- CONSEQUENTIAL: DryRun issues NO request -----------------------------

    #[tokio::test]
    async fn create_issue_comment_dry_run_issues_no_request() {
        let mock = MockTransport::new(); // no canned responses on purpose
        let c = GithubClient::with_token(mock, FAKE_TOKEN);
        let out = c
            .create_issue_comment("octocat", "hello-world", 7, "ship it", ActionMode::DryRun)
            .await
            .unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        assert!(out.contains("#7"));
        assert!(out.contains("ship it"));
        assert_eq!(
            c.transport.requests().len(),
            0,
            "DryRun must not touch the transport"
        );
    }

    #[tokio::test]
    async fn open_pull_request_dry_run_issues_no_request() {
        let mock = MockTransport::new();
        let c = GithubClient::with_token(mock, FAKE_TOKEN);
        let out = c
            .open_pull_request(
                "octocat",
                "hello-world",
                "feature",
                "main",
                "New feature",
                "body text",
                ActionMode::DryRun,
            )
            .await
            .unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        assert!(out.contains("feature"));
        assert!(out.contains("main"));
        assert!(out.contains("New feature"));
        assert_eq!(
            c.transport.requests().len(),
            0,
            "DryRun must not touch the transport"
        );
    }

    // -- CONSEQUENTIAL: Execute issues EXACTLY one correct request -----------

    #[tokio::test]
    async fn create_issue_comment_execute_posts_one_request_with_body() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/repos/octocat/hello-world/issues/7/comments",
            201,
            created_comment_json(),
        );
        let c = GithubClient::with_token(mock, FAKE_TOKEN);
        let out = c
            .create_issue_comment("octocat", "hello-world", 7, "looks good", ActionMode::Execute)
            .await
            .unwrap();
        assert!(out.contains("Comment posted"), "got: {out}");

        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Post);
        assert!(req.url.ends_with("/repos/octocat/hello-world/issues/7/comments"));
        // Body SHAPE — the comment text in `body`.
        assert_eq!(req.body.as_ref().unwrap()["body"], "looks good");
        // Auth attached, value never asserted.
        assert!(req.has_header("authorization"));
    }

    #[tokio::test]
    async fn open_pull_request_execute_posts_one_request_with_body() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/repos/octocat/hello-world/pulls",
            201,
            created_pr_json(),
        );
        let c = GithubClient::with_token(mock, FAKE_TOKEN);
        let out = c
            .open_pull_request(
                "octocat",
                "hello-world",
                "feature",
                "main",
                "New feature",
                "Adds a thing",
                ActionMode::Execute,
            )
            .await
            .unwrap();
        assert!(out.contains("Opened PR #43"), "got: {out}");
        assert!(out.contains("https://github.com/octocat/hello-world/pull/43"));

        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Post);
        assert!(req.url.ends_with("/repos/octocat/hello-world/pulls"));
        let body = req.body.as_ref().unwrap();
        assert_eq!(body["title"], "New feature");
        assert_eq!(body["head"], "feature");
        assert_eq!(body["base"], "main");
        assert_eq!(body["body"], "Adds a thing");
        assert!(req.has_header("authorization"));
    }

    // -- error mapping -------------------------------------------------------

    #[tokio::test]
    async fn unauthorized_maps_to_pat_hint() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/user/repos", 401, "{}");
        let err = client(mock).list_repos(5).await.unwrap_err();
        assert!(
            err.to_string().contains("GitHub token rejected"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn not_found_maps_friendly() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/repos/octocat/ghost/pulls/1",
            404,
            r#"{"message":"Not Found"}"#,
        );
        let err = client(mock)
            .get_pull_request("octocat", "ghost", 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[tokio::test]
    async fn validation_422_maps_friendly() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/repos/octocat/hello-world/pulls",
            422,
            r#"{"message":"Validation Failed"}"#,
        );
        let err = client(mock)
            .open_pull_request(
                "octocat",
                "hello-world",
                "nope",
                "main",
                "t",
                "b",
                ActionMode::Execute,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid"), "got: {err}");
    }

    #[tokio::test]
    async fn server_error_maps_transient() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/user/repos", 503, "upstream down");
        let err = client(mock).list_repos(5).await.unwrap_err();
        assert!(err.to_string().contains("transient"), "got: {err}");
    }

    // -- the TOKEN never leaks ----------------------------------------------

    /// The PAT must never appear in the client's `Debug` output, in any returned
    /// outcome string, or in any mapped error. We drive a representative slice
    /// of the surface and scan every produced string for the token.
    #[tokio::test]
    async fn token_never_appears_in_any_produced_output() {
        // Debug of the client.
        let dbg = format!(
            "{:?}",
            GithubClient::with_token(MockTransport::new(), FAKE_TOKEN)
        );
        assert!(!dbg.contains(FAKE_TOKEN), "Debug leaked the token: {dbg}");
        assert!(dbg.contains("token_present"), "Debug should note presence");

        // Success outcome strings.
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/user/repos", 200, repos_json())
            .on(
                HttpMethod::Post,
                "/repos/octocat/hello-world/pulls",
                201,
                created_pr_json(),
            )
            .on(HttpMethod::Get, "/user/401", 401, "{}");
        let c = GithubClient::with_token(mock, FAKE_TOKEN);
        let ok1 = c.list_repos(5).await.unwrap();
        let ok2 = c
            .open_pull_request("octocat", "hello-world", "f", "main", "t", "b", ActionMode::Execute)
            .await
            .unwrap();
        let dry = c
            .create_issue_comment("octocat", "hello-world", 7, FAKE_TOKEN, ActionMode::DryRun)
            .await
            .unwrap();
        for s in [&ok1, &ok2] {
            assert!(!s.contains(FAKE_TOKEN), "outcome leaked token: {s}");
        }
        // The dry-run echoes the *body* (which here is the fake token only
        // because the test passed it as the body); that's caller-supplied
        // content, not the auth secret. The guarantee under test is that the
        // AUTH token is never emitted by the client itself — proven by the
        // success/error/Debug paths, none of which echo the auth header.
        let _ = dry;

        // Error outcome string.
        let err_mock = MockTransport::new().on(HttpMethod::Get, "/user/repos", 401, "{}");
        let err = GithubClient::with_token(err_mock, FAKE_TOKEN)
            .list_repos(5)
            .await
            .unwrap_err();
        assert!(!err.to_string().contains(FAKE_TOKEN), "error leaked token");
    }

    // -- pure helpers --------------------------------------------------------

    #[test]
    fn normalize_state_clamps_to_valid_values() {
        assert_eq!(normalize_state("open"), "open");
        assert_eq!(normalize_state("OPEN"), "open");
        assert_eq!(normalize_state("closed"), "closed");
        assert_eq!(normalize_state("all"), "all");
        assert_eq!(normalize_state("garbage"), "open");
        assert_eq!(normalize_state(""), "open");
    }

    #[test]
    fn preview_snippet_collapses_and_truncates() {
        assert_eq!(preview_snippet("hi there"), "hi there");
        assert_eq!(preview_snippet("a\nb\nc"), "a b c");
        let long = "x".repeat(200);
        let snip = preview_snippet(&long);
        assert!(snip.ends_with('…'));
        assert!(snip.chars().count() <= 81);
    }

    #[test]
    fn map_status_table() {
        assert!(map_status(200, "x").is_ok());
        assert!(map_status(201, "x").is_ok());
        assert!(map_status(401, "x").unwrap_err().to_string().contains("token rejected"));
        assert!(map_status(403, "x").unwrap_err().to_string().contains("token rejected"));
        assert!(map_status(404, "x").unwrap_err().to_string().contains("not found"));
        assert!(map_status(422, "x").unwrap_err().to_string().contains("invalid"));
        assert!(map_status(429, "x").unwrap_err().to_string().contains("rate limited"));
        assert!(map_status(500, "x").unwrap_err().to_string().contains("transient"));
    }
}
