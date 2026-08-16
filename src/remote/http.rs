//! The blocking HTTP client every `br remote` backend call goes through.
//!
//! Blocking on purpose: an async runtime in a synchronous CLI is a larger
//! change than the feature that would need it.

use crate::remote::config::RemoteConfig;
use crate::remote::error::RemoteError;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// The only place a credential is read from.
pub const TOKEN_ENV: &str = "BR_YOUTRACK_TOKEN";

/// A bearer token that cannot be printed.
///
/// No `Display`, and a `Debug` that redacts. Everything br writes — dry-run
/// plans, tracing spans, error messages — goes through one of those two, so
/// making them safe makes the whole surface safe.
///
/// The inner value is private for the same reason: a `pub` field would let
/// any caller print the secret via `token.0` regardless of `Debug`, so the
/// only way out of this type is `authorization_header`, which returns the
/// one string this crate actually needs to send — `"Bearer <token>"` — never
/// the bare token.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Wrap a raw token value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The `Authorization` header value this token authenticates with.
    ///
    /// This is the only way to get anything derived from the raw secret out
    /// of a `Token`, and what it returns is the header value, not the token
    /// itself.
    #[must_use]
    pub fn authorization_header(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Read the token from the environment.
///
/// # Errors
/// Returns `RemoteError::Config` when the variable is unset or empty.
pub fn token_from_env() -> Result<Token, RemoteError> {
    token_from(std::env::var(TOKEN_ENV).ok())
}

/// The validation `token_from_env` performs, pulled out as a pure function.
///
/// This crate forbids `unsafe_code` (`src/lib.rs`), and current `std` makes
/// `env::remove_var` an `unsafe fn` (it is not safe under a multi-threaded
/// process). A unit test therefore cannot mutate the real process
/// environment to exercise the "unset" branch. Testing this pure function
/// with `None` in its place covers the same branch without an env mutation
/// — and without a `forbid`-defeating `unsafe` block, which `#[allow]`
/// cannot override at any nested scope.
fn token_from(value: Option<String>) -> Result<Token, RemoteError> {
    match value {
        Some(value) if !value.trim().is_empty() => Ok(Token::new(value)),
        _ => Err(RemoteError::Config(format!(
            "{TOKEN_ENV} is not set. `br remote` reads its credential from that \
             variable and from nowhere else, so no file br writes can leak it. \
             Set it first, e.g. `export {TOKEN_ENV}=perm-…`."
        ))),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
        }
    }
}

impl RetryPolicy {
    /// A policy that never retries. Used by tests that assert an exact
    /// request sequence, where a silent retry would make the assertion lie.
    #[must_use]
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            base_delay: Duration::ZERO,
        }
    }

    /// Exponential backoff: `base * 2^attempt`.
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> Duration {
        self.base_delay * 2_u32.saturating_pow(attempt)
    }

    /// Whether `err` on zero-based `attempt` warrants another try.
    #[must_use]
    pub fn should_retry(&self, err: &RemoteError, attempt: u32) -> bool {
        err.is_retryable() && attempt + 1 < self.max_attempts
    }
}

#[derive(Debug)]
pub struct HttpClient {
    base_url: String,
    token: Token,
    retry: RetryPolicy,
    agent: ureq::Agent,
    reads: AtomicU32,
    writes: AtomicU32,
}

impl HttpClient {
    #[must_use]
    pub fn new(base_url: &str, token: Token, retry: RetryPolicy) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            retry,
            agent: ureq::Agent::new_with_defaults(),
            reads: AtomicU32::new(0),
            writes: AtomicU32::new(0),
        }
    }

    /// Build a client for `cfg`, taking the credential from the environment.
    ///
    /// # Errors
    /// Returns `RemoteError::Config` when the token is unset.
    pub fn from_env(cfg: &RemoteConfig) -> Result<Self, RemoteError> {
        Ok(Self::new(
            &cfg.url,
            token_from_env()?,
            RetryPolicy::default(),
        ))
    }

    /// Requests issued that could not change remote state.
    #[must_use]
    pub fn read_count(&self) -> u32 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Requests issued that could change remote state. `br remote status`
    /// asserts this is zero; so do several provisioning tests.
    #[must_use]
    pub fn write_count(&self) -> u32 {
        self.writes.load(Ordering::Relaxed)
    }

    /// GET `path`, returning the parsed JSON body.
    ///
    /// # Errors
    /// Returns `RemoteError` on transport failure or a non-2xx response,
    /// after exhausting the retry policy.
    pub fn get_json(&self, path: &str, entity: &str) -> Result<serde_json::Value, RemoteError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.send("GET", path, None, entity)
    }

    /// POST `body` to `path`, returning the parsed JSON response.
    ///
    /// # Errors
    /// As `get_json`.
    pub fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
        entity: &str,
    ) -> Result<serde_json::Value, RemoteError> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.send("POST", path, Some(body), entity)
    }

    /// DELETE `path`.
    ///
    /// # Errors
    /// As `get_json`.
    pub fn delete(&self, path: &str, entity: &str) -> Result<(), RemoteError> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.send("DELETE", path, None, entity).map(|_| ())
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        entity: &str,
    ) -> Result<serde_json::Value, RemoteError> {
        let url = format!("{}{path}", self.base_url);
        let mut attempt = 0;
        loop {
            match self.send_once(method, &url, body, entity) {
                Ok(value) => return Ok(value),
                Err(err) if self.retry.should_retry(&err, attempt) => {
                    std::thread::sleep(self.retry.delay_for(attempt));
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Issue a single request and classify the response.
    ///
    /// `ureq` 3's `Error::StatusCode(u16)` variant does not carry the
    /// response body, so the default status-to-error translation is turned
    /// off per-request (`.config().http_status_as_error(false)`) and the
    /// status is read from the successful `Response` instead. That keeps the
    /// body available for `RemoteError::from_response`, which needs both to
    /// recognise a `must-be-unique` conflict.
    fn send_once(
        &self,
        method: &str,
        url: &str,
        body: Option<&serde_json::Value>,
        entity: &str,
    ) -> Result<serde_json::Value, RemoteError> {
        let auth = self.token.authorization_header();
        let response = match method {
            "GET" => with_common_headers(self.agent.get(url), &auth).call(),
            "DELETE" => with_common_headers(self.agent.delete(url), &auth).call(),
            "POST" => {
                let request = with_common_headers(self.agent.post(url), &auth)
                    .header("Content-Type", "application/json");
                // `ureq`'s own `json` feature is deliberately not enabled
                // (see the comment on the `ureq` dependency in `Cargo.toml`),
                // so the body is serialized with the crate's own `serde_json`
                // and sent as bytes rather than through `RequestBuilder::send_json`.
                match body {
                    Some(value) => {
                        let bytes = serde_json::to_vec(value).map_err(|e| {
                            RemoteError::Transport(format!("request body was not JSON: {e}"))
                        })?;
                        request.send(bytes.as_slice())
                    }
                    None => request.send_empty(),
                }
            }
            other => {
                return Err(RemoteError::Transport(format!(
                    "unsupported HTTP method: {other}"
                )));
            }
        };

        match response {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let text = resp
                    .body_mut()
                    .read_to_string()
                    .map_err(|e| RemoteError::Transport(e.to_string()))?;
                if !(200..300).contains(&status) {
                    return Err(RemoteError::from_response(status, &text, entity));
                }
                if text.trim().is_empty() {
                    return Ok(serde_json::Value::Null);
                }
                serde_json::from_str(&text)
                    .map_err(|e| RemoteError::Transport(format!("response was not JSON: {e}")))
            }
            Err(e) => Err(RemoteError::Transport(e.to_string())),
        }
    }
}

/// The request configuration and headers every method needs alike:
/// `http_status_as_error(false)` (see `HttpClient::send_once`'s doc comment
/// for why) plus the bearer token and `Accept` header. Generic over `S` so
/// it takes both `RequestBuilder<WithoutBody>` (GET, DELETE) and
/// `RequestBuilder<WithBody>` (POST) without duplicating the chain per
/// method.
fn with_common_headers<S>(request: ureq::RequestBuilder<S>, auth: &str) -> ureq::RequestBuilder<S> {
    request
        .config()
        .http_status_as_error(false)
        .build()
        .header("Authorization", auth)
        .header("Accept", "application/json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_token_fails_before_any_request() {
        // `token_from(None)` is what `token_from_env()` reduces to when
        // `BR_YOUTRACK_TOKEN` is unset — see the comment on `token_from` for
        // why the test does not mutate the real process environment.
        let err = token_from(None).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains(TOKEN_ENV), "must name the variable: {msg}");
        assert!(msg.contains("export"), "must say how to fix it: {msg}");
    }

    #[test]
    fn the_token_is_never_printable() {
        let token = Token::new("super-secret-value");
        let debug = format!("{token:?}");
        assert!(
            !debug.contains("super-secret-value"),
            "Debug leaked the token: {debug}"
        );
        assert!(
            debug.contains("redacted"),
            "Debug should say it is redacted: {debug}"
        );

        let client = HttpClient::new("https://example.invalid", token, RetryPolicy::none());
        let debug = format!("{client:?}");
        assert!(
            !debug.contains("super-secret-value"),
            "client Debug leaked the token: {debug}"
        );
    }

    #[test]
    fn backoff_delays_grow_and_stop_at_max_attempts() {
        let policy = RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(10),
        };
        let delays: Vec<u64> = (0..3)
            .map(|attempt| {
                u64::try_from(policy.delay_for(attempt).as_millis())
                    .expect("test delay fits in u64")
            })
            .collect();
        assert_eq!(delays, vec![10, 20, 40], "delays must double");
        assert_eq!(policy.max_attempts, 4);
    }

    #[test]
    fn only_retryable_errors_are_retried() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
        };
        assert!(policy.should_retry(
            &RemoteError::Http {
                status: 429,
                body: String::new()
            },
            0
        ));
        assert!(policy.should_retry(
            &RemoteError::Http {
                status: 503,
                body: String::new()
            },
            0
        ));
        assert!(!policy.should_retry(
            &RemoteError::Http {
                status: 403,
                body: String::new()
            },
            0
        ));
        // Attempts are exhausted even for a retryable status.
        assert!(!policy.should_retry(
            &RemoteError::Http {
                status: 429,
                body: String::new()
            },
            2
        ));
    }

    // The tests below drive `HttpClient` end to end over the loopback mock
    // server in `test_support::mock_http`, rather than only exercising the
    // pure `RetryPolicy`/`RemoteError` helpers above. Nothing here resolves a
    // real host: `MockServer::start()` binds `127.0.0.1:0`.

    use std::time::Instant;
    use test_support::mock_http::MockServer;

    fn client(server: &MockServer, retry: RetryPolicy) -> HttpClient {
        HttpClient::new(&server.base_url(), Token::new("test-token"), retry)
    }

    #[test]
    fn a_429_then_200_succeeds_after_exactly_one_slept_retry() {
        let server = MockServer::start();
        server.on_sequence(
            "GET",
            "/api/issues",
            vec![
                (429, "rate limited".to_string()),
                (200, r#"{"ok":true}"#.to_string()),
            ],
        );
        let base_delay = Duration::from_millis(30);
        let http = client(
            &server,
            RetryPolicy {
                max_attempts: 3,
                base_delay,
            },
        );

        let start = Instant::now();
        let value = http
            .get_json("/api/issues", "issue EM-1")
            .expect("must succeed on retry");
        let elapsed = start.elapsed();

        assert_eq!(value, serde_json::json!({"ok": true}));
        assert_eq!(
            server.requests().len(),
            2,
            "the 429 and the 200 that follows it"
        );
        assert!(
            elapsed >= base_delay,
            "the backoff before the retry must actually have been slept: elapsed {elapsed:?}, wanted at least {base_delay:?}"
        );
    }

    #[test]
    fn a_500_sequence_exhausts_max_attempts_and_returns_the_final_http_error() {
        let server = MockServer::start();
        server.on(
            "GET",
            "/api/issues",
            500,
            r#"{"error":"Internal Server Error"}"#,
        );
        let http = client(
            &server,
            RetryPolicy {
                max_attempts: 3,
                base_delay: Duration::from_millis(1),
            },
        );

        let err = http
            .get_json("/api/issues", "issue EM-1")
            .expect_err("must exhaust retries");

        assert!(
            matches!(err, RemoteError::Http { status: 500, .. }),
            "got {err:?}"
        );
        assert_eq!(
            server.requests().len(),
            3,
            "exactly max_attempts requests, not one more and not one fewer"
        );
    }

    #[test]
    fn a_400_is_not_retried() {
        let server = MockServer::start();
        server.on("GET", "/api/issues", 400, r#"{"error":"Bad Request"}"#);
        let http = client(
            &server,
            RetryPolicy {
                max_attempts: 5,
                base_delay: Duration::from_millis(1),
            },
        );

        let err = http
            .get_json("/api/issues", "issue EM-1")
            .expect_err("must fail");

        assert!(
            matches!(err, RemoteError::Http { status: 400, .. }),
            "got {err:?}"
        );
        assert_eq!(
            server.requests().len(),
            1,
            "a non-retryable status must not be retried"
        );
    }

    #[test]
    fn write_count_is_zero_after_gets_and_counts_post_and_delete() {
        let server = MockServer::start();
        server.on("GET", "/api/issues", 200, r#"[{"id":"3-1"}]"#);
        server.on("POST", "/api/issues", 200, r#"{"idReadable":"EM-1"}"#);
        server.on("DELETE", "/api/issueLinks/EM-5", 200, "");
        let http = client(&server, RetryPolicy::none());

        http.get_json("/api/issues", "issue").expect("get");
        http.get_json("/api/issues", "issue").expect("get");
        assert_eq!(http.read_count(), 2);
        assert_eq!(http.write_count(), 0, "GETs must never count as writes");

        http.post_json("/api/issues", &serde_json::json!({"summary": "x"}), "issue")
            .expect("post");
        assert_eq!(http.write_count(), 1);

        http.delete("/api/issueLinks/EM-5", "link").expect("delete");
        assert_eq!(
            http.write_count(),
            2,
            "POST and DELETE both count as writes"
        );
        assert_eq!(http.read_count(), 2, "writes must not also count as reads");
    }

    #[test]
    fn no_remote_error_variant_can_leak_the_token() {
        let secret = "super-secret-value";

        // `RemoteError::Http`, reached through a real non-2xx response.
        let server = MockServer::start();
        server.on("GET", "/api/issues", 500, r#"{"error":"boom"}"#);
        let http = HttpClient::new(&server.base_url(), Token::new(secret), RetryPolicy::none());
        let http_err = http
            .get_json("/api/issues", "issue")
            .expect_err("500 must error");
        assert!(matches!(http_err, RemoteError::Http { status: 500, .. }));
        assert!(
            !http_err.to_string().contains(secret),
            "Display leaked the token: {http_err}"
        );
        assert!(
            !format!("{http_err:?}").contains(secret),
            "Debug leaked the token: {http_err:?}"
        );

        // `RemoteError::Transport`, reached because nothing is listening on
        // this port: bind then immediately drop, so the connect attempt
        // fails fast with "connection refused" instead of a slow timeout.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let dead_addr = listener.local_addr().expect("local addr");
        drop(listener);
        let dead = HttpClient::new(
            &format!("http://{dead_addr}"),
            Token::new(secret),
            RetryPolicy::none(),
        );
        let transport_err = dead
            .get_json("/api/issues", "issue")
            .expect_err("must fail to connect");
        assert!(
            matches!(transport_err, RemoteError::Transport(_)),
            "got {transport_err:?}"
        );
        assert!(
            !transport_err.to_string().contains(secret),
            "Display leaked the token: {transport_err}"
        );
        assert!(
            !format!("{transport_err:?}").contains(secret),
            "Debug leaked the token: {transport_err:?}"
        );

        // `RemoteError::AlreadyExists` and `RemoteError::Config` never wrap
        // arbitrary network-echoed data that could carry a token in the
        // first place: `AlreadyExists.entity` and `Config`'s message are
        // both written by this crate's own code, not read back off the wire.
    }

    #[test]
    fn the_authorization_header_reaches_the_server() {
        let server = MockServer::start();
        server.on("GET", "/api/issues", 200, r#"[{"id":"3-1"}]"#);
        let http = HttpClient::new(
            &server.base_url(),
            Token::new("super-secret-value"),
            RetryPolicy::none(),
        );

        http.get_json("/api/issues", "issue").expect("get");

        let recorded = server.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].auth.as_deref(),
            Some("Bearer super-secret-value"),
            "the token must actually transmit, even though it can never be printed"
        );
    }
}
