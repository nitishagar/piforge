//! OpenAI-compatible chat-completion client. Points at any `/v1/chat/completions`
//! endpoint — a local llama-server/cactus OR a cloud API-key provider (z.ai/GLM,
//! OpenAI, Kimi, OpenRouter …) — selected purely by `base_url` + `api_key` +
//! `model` in [`crate::config::ServerConfig`]. Sends `tools` + `tool_choice`,
//! reports token usage incl. the cache telemetry that verifies the prefix-cache
//! premise, and applies bounded retry on transient failures (429 / 5xx / a
//! transient transport error) so a single rate-limit doesn't fail an eval case.
use std::error::Error;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};

use crate::config::ServerConfig;

/// Retry policy for transient chat-completion failures. Bounded exponential
/// backoff; production defaults live in [`DEFAULT_RETRY`]. Split out so the
/// retry tests can run with sub-second delays instead of the real ~1–7s.
#[derive(Debug, Clone, Copy)]
struct RetryConfig {
    /// Number of retries after the first attempt (total attempts = max_retries + 1).
    max_retries: u32,
    /// Base backoff; delay before retry `n` (1-indexed) is `base_delay * 2^(n-1)`.
    base_delay: Duration,
    /// Per-sleep cap (no single backoff exceeds this).
    max_delay: Duration,
}

/// Production retry policy: up to 3 retries (4 attempts) with 1s/2s/4s backoff
/// (+ sub-second jitter, each sleep capped at 30s). Total added wait ≤ ~15s.
const DEFAULT_RETRY: RetryConfig = RetryConfig {
    max_retries: 3,
    base_delay: Duration::from_secs(1),
    max_delay: Duration::from_secs(30),
};

/// The chat-completion client.
pub struct Client {
    http: HttpClient,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    temperature: f32,
    retry: RetryConfig,
    metrics: parking_lot::Mutex<Metrics>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Metrics {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub calls: u64,
}

impl Client {
    pub fn new(cfg: &ServerConfig) -> Result<Self> {
        Self::build(cfg, DEFAULT_RETRY, Duration::from_secs(300))
    }

    /// Construct with an explicit retry policy + HTTP timeout. Private;
    /// `Client::new` uses the production defaults. The inline tests pass tiny
    /// delays (fast retries) and a short timeout (deterministic transient-
    /// timeout retry) here so they stay fast and non-flaky.
    fn build(cfg: &ServerConfig, retry: RetryConfig, timeout: Duration) -> Result<Self> {
        let http = HttpClient::builder()
            .timeout(timeout) // a slow Pi/cloud can take minutes/turn
            .build()?;
        Ok(Self {
            http,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
            retry,
            metrics: parking_lot::Mutex::new(Metrics::default()),
        })
    }

    /// Health check: authenticated `GET /models` to confirm the server is up and
    /// serving the OpenAI shape. Cloud `/v1/models` endpoints require the Bearer
    /// token (a local llama-server ignores it), so this sends `bearer_auth` just
    /// like [`chat`](Self::chat). The error names `base_url`, and on 401/403 it
    /// names `PIFORGE_API_KEY` so a wrong/missing key is obvious rather than an
    /// opaque "HTTP 401".
    pub async fn health_check(&self) -> Result<()> {
        let url = format!("{}/models", self.base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await?;
        let status = resp.status();
        // Always drain so the connection is released cleanly (uniform on both
        // paths); a /models body is small and this is a one-shot startup check.
        let _ = resp.text().await;
        if status.is_success() {
            return Ok(());
        }
        let code = status.as_u16();
        let msg = match code {
            401 | 403 => format!(
                "server health check failed at {} (HTTP {}): auth denied — check PIFORGE_API_KEY",
                self.base_url, code
            ),
            _ => format!(
                "server health check failed at {} (HTTP {})",
                self.base_url, code
            ),
        };
        Err(anyhow!(msg))
    }

    /// Non-streaming chat completion. Returns content + tool calls + telemetry.
    ///
    /// Transient failures are retried with bounded exponential backoff: HTTP
    /// `429` or `>=500`, or a transient transport error (timeout / connection
    /// reset / broken pipe), up to [`RetryConfig::max_retries`] retries. Any
    /// other `4xx` (incl. 400/401/403/404) and non-transient transport errors
    /// fail fast — 401/403 name `PIFORGE_API_KEY` + `base_url` so a bad key is
    /// obvious. The signature is unchanged: retry is internal, so the agent loop
    /// and eval gate are unaware of it (Inv 1/2 preserved).
    pub async fn chat(&self, req: &ChatRequest) -> Result<ChatResponse> {
        let body = ChatCompletionRequest {
            // A local llama-server ignores `model` (serves its loaded GGUF); a
            // cloud provider requires the real id from ServerConfig.model.
            model: self.model.clone(),
            messages: req.messages.clone(),
            tools: if req.tools.is_empty() {
                None
            } else {
                Some(req.tools.clone())
            },
            tool_choice: req.tool_choice.clone(),
            max_tokens: req.max_tokens.unwrap_or(self.max_tokens),
            temperature: self.temperature,
            stream: false,
        };
        let url = format!("{}/chat/completions", self.base_url);

        let mut attempt: u32 = 0;
        loop {
            let send = self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await;
            match send {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return self.finish_chat(resp).await;
                    }
                    let code = status.as_u16();
                    // Retry on 429 / 5xx; fail fast on other 4xx.
                    if (code == 429 || code >= 500) && attempt < self.retry.max_retries {
                        let _ = resp.text().await; // drain to release the connection
                        attempt += 1;
                        self.backoff(attempt).await;
                        continue;
                    }
                    let text = resp.text().await.unwrap_or_default();
                    return Err(self.actionable_error(code, &text));
                }
                Err(err) => {
                    // Retry transient transport errors; fail fast otherwise.
                    if is_transient_transport(&err) && attempt < self.retry.max_retries {
                        attempt += 1;
                        self.backoff(attempt).await;
                        continue;
                    }
                    return Err(anyhow!("chat completion: request failed: {err}"));
                }
            }
        }
    }

    /// Parse a successful response into [`ChatResponse`] and update metrics.
    async fn finish_chat(&self, resp: reqwest::Response) -> Result<ChatResponse> {
        let cc: ChatCompletionResponse = resp.json().await?;
        let choice = cc.choices.first().ok_or_else(|| anyhow!("empty choices"))?;
        let out = ChatResponse {
            content: choice.message.content.clone().unwrap_or_default(),
            tool_calls: choice.message.tool_calls.clone().unwrap_or_default(),
            finish_reason: choice.finish_reason.clone().unwrap_or_default(),
            prompt_tokens: cc.usage.prompt_tokens,
            completion: cc.usage.completion_tokens,
            cached: cc
                .usage
                .prompt_tokens_details
                .as_ref()
                .map_or(0, |d| d.cached_tokens),
        };
        {
            let mut m = self.metrics.lock();
            m.prompt_tokens += out.prompt_tokens;
            m.completion_tokens += out.completion;
            m.cached_tokens += out.cached;
            m.calls += 1;
        }
        Ok(out)
    }

    /// Backoff before retry `attempt` (1-indexed): `base_delay * 2^(attempt-1)`
    /// capped at `max_delay`, plus up to `base_delay` of jitter.
    async fn backoff(&self, attempt: u32) {
        let base = self.retry.base_delay;
        let exp = base.saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)));
        let capped = exp.min(self.retry.max_delay);
        tokio::time::sleep(capped + jitter_up_to(base)).await;
    }

    /// Build an actionable error for a non-retryable HTTP status. 401/403 name
    /// `PIFORGE_API_KEY` + `base_url`. A provider/gateway can echo request
    /// headers (incl. the Authorization bearer) back in its response body, so
    /// the body is scrubbed of the key value and truncated before it touches an
    /// error string — the key VALUE never appears, only the env-var NAME.
    fn actionable_error(&self, code: u16, body: &str) -> anyhow::Error {
        let redacted = body.replace(self.api_key.as_str(), "[redacted]");
        let snippet: String = redacted.chars().take(300).collect();
        match code {
            401 | 403 => anyhow!(
                "chat completion: HTTP {code} at {}: auth denied — check PIFORGE_API_KEY (body: {snippet})",
                self.base_url
            ),
            _ => anyhow!("chat completion: HTTP {code}: {snippet}"),
        }
    }

    pub fn metrics(&self) -> Metrics {
        *self.metrics.lock()
    }
}

/// Classify a transport error as transient (worth retrying). Covers reqwest's
/// own `is_timeout()`/`is_connect()` AND the transient `std::io::Error` kinds
/// that reqwest surfaces from hyper when a connection is reset or dropped
/// mid-request (which `is_connect()` alone does NOT flag — a plain reset after
/// connect is a hyper/io error, not a Connect-kind error). Non-transient causes
/// (bad URL, decode/JSON error, DNS) are intentionally excluded.
fn is_transient_transport(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() {
        return true;
    }
    let mut source: Option<&(dyn Error + 'static)> = err.source();
    while let Some(e) = source {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::WouldBlock => return true,
                _ => {}
            }
        }
        source = e.source();
    }
    false
}

/// A jitter duration in `[0, max)`, drawn from `SystemTime` nanos so no extra
/// dependency is needed. The eval loop is serial, so there is no herd to
/// decorrelate — jitter honors the plan's backoff formula and guards against
/// pathological alignment under retry.
fn jitter_up_to(max: Duration) -> Duration {
    let max_ns = max.as_nanos() as u64;
    if max_ns == 0 {
        return Duration::ZERO;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Duration::from_nanos(nanos % max_ns)
}

/// Input to a chat completion.
#[derive(Clone)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<Tool>,
    pub tool_choice: Option<serde_json::Value>, // "auto" | "none" | {"type":"function","function":{"name":x}}
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

/// Output of a chat completion, with telemetry.
#[derive(Debug)]
pub struct ChatResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    pub prompt_tokens: u64,
    pub completion: u64,
    pub cached: u64,
}

// ---- Wire types for the OpenAI-compat request/response ----

#[derive(Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    max_tokens: u32,
    temperature: f32,
    stream: bool,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
    usage: Usage,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Deserialize)]
struct Usage {
    prompt_tokens: u64,
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[cfg(test)]
mod tests {
    //! Provider retry + health-check tests. A loopback `TcpListener` stub serves
    //! canned responses in order, one per accepted connection (dispatched to its
    //! own task so a hanging/reset response can't block the accept loop). No new
    //! dependency. Inv 7 (retry) + Inv 8 (authed health check) are covered here.
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// What the stub recorded about one request.
    #[derive(Debug, Clone)]
    struct Seen {
        request_line: String,
        auth_header: Option<String>,
    }

    /// A canned response the stub serves on its next accepted connection.
    #[derive(Debug, Clone)]
    enum Stub {
        /// HTTP status; 200 carries a minimal valid OpenAI response body.
        Status(u16),
        /// HTTP status with a custom body (used to test that the key value is
        /// scrubbed from surfaced error bodies).
        Body { code: u16, body: String },
        /// Read one byte then close (forces a TCP RST — a transient transport error).
        Reset,
        /// Accept and never respond (the client's request times out).
        Hang,
    }

    /// A running stub server: bind a loopback listener, serve `responses` in
    /// order (one connection each), record each request. `Connection: close`
    /// forces a fresh connection per attempt so retries hit the next response.
    async fn spawn_stub(responses: Vec<Stub>) -> (std::net::SocketAddr, Arc<Mutex<Vec<Seen>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        tokio::spawn(async move {
            let mut iter = responses.into_iter();
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let Some(resp) = iter.next() else { return };
                let seen = seen_clone.clone();
                tokio::spawn(async move {
                    serve(&mut sock, resp, seen).await;
                });
            }
        });
        (addr, seen)
    }

    async fn serve(sock: &mut tokio::net::TcpStream, resp: Stub, seen: Arc<Mutex<Vec<Seen>>>) {
        match resp {
            Stub::Reset => {
                // Read one byte (so we know the request started arriving), then
                // drop WITHOUT reading the rest: unread received data on close
                // makes the kernel send RST (not FIN) — a transient
                // ConnectionReset that retry must handle.
                let mut b = [0u8; 1];
                let _ = sock.read(&mut b).await;
                // Record the attempt so tests can count retries (no full request).
                seen.lock().unwrap().push(Seen {
                    request_line: "<reset>".into(),
                    auth_header: None,
                });
            }
            Stub::Hang => {
                // Record the attempt, then hold the connection open without ever
                // responding; the client's request times out (is_timeout() path).
                seen.lock().unwrap().push(Seen {
                    request_line: "<hang>".into(),
                    auth_header: None,
                });
                std::future::pending::<()>().await;
            }
            Stub::Status(code) => {
                let body = status_body(code);
                respond(sock, code, &body, &seen).await;
            }
            Stub::Body { code, body } => {
                respond(sock, code, &body, &seen).await;
            }
        }
    }

    /// Read the request, record it, and write back a canned HTTP/1.1 response.
    async fn respond(
        sock: &mut tokio::net::TcpStream,
        code: u16,
        body: &str,
        seen: &Arc<Mutex<Vec<Seen>>>,
    ) {
        let s = read_request(sock).await;
        seen.lock().unwrap().push(s);
        let out = format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\n\
             Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
            reason = reason_phrase(code),
            len = body.len(),
        );
        sock.write_all(out.as_bytes()).await.ok();
        sock.flush().await.ok();
    }

    /// Read one full HTTP request (request line + headers + Content-Length body).
    async fn read_request(sock: &mut tokio::net::TcpStream) -> Seen {
        let mut buf: Vec<u8> = Vec::with_capacity(2048);
        let mut chunk = [0u8; 2048];
        loop {
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if find_subslice(&buf, b"\r\n\r\n").is_some() {
                break;
            }
        }
        let headers_end = find_subslice(&buf, b"\r\n\r\n")
            .map(|p| p + 4)
            .unwrap_or(buf.len());
        let header_str = String::from_utf8_lossy(&buf[..headers_end]).to_string();
        let content_len = content_length(&header_str);
        // Drain the body so our response write doesn't race with the client send.
        let mut have = buf.len().saturating_sub(headers_end);
        while have < content_len {
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            have += n;
        }
        let request_line = header_str.lines().next().unwrap_or("").to_string();
        let auth_header = header_str
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
            .map(|l| l.trim().to_string());
        Seen {
            request_line,
            auth_header,
        }
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|l| {
                let low = l.trim().to_ascii_lowercase();
                low.strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0)
    }

    fn reason_phrase(code: u16) -> &'static str {
        match code {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            429 => "Too Many Requests",
            500 | 503 => "Server Error",
            _ => "OK",
        }
    }

    fn status_body(code: u16) -> String {
        if code == 200 {
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#
                .to_string()
        } else {
            format!("{{\"error\":\"stub status {code}\"}}")
        }
    }

    fn cfg_for(addr: std::net::SocketAddr) -> ServerConfig {
        ServerConfig {
            base_url: format!("http://{addr}"),
            api_key: "test-key".into(),
            model: "piforge".into(),
            provider: String::new(),
            max_tokens: 64,
            temperature: 0.0,
        }
    }

    /// A minimal chat request (no tools) sufficient to drive `chat()`.
    fn chat_req() -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: Some("hi".into()),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: None,
        }
    }

    fn fast_retry() -> RetryConfig {
        RetryConfig {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
    }

    #[tokio::test]
    async fn health_check_sends_bearer_and_ok_on_200() {
        let (addr, seen) = spawn_stub(vec![Stub::Status(200)]).await;
        let client = Client::new(&cfg_for(addr)).unwrap();
        client.health_check().await.expect("200 should be Ok");
        let s = seen.lock().unwrap();
        assert_eq!(s.len(), 1, "exactly one request");
        assert!(
            s[0].request_line.starts_with("GET /models"),
            "got: {}",
            s[0].request_line
        );
        let auth = s[0]
            .auth_header
            .as_deref()
            .expect("bearer auth must be sent");
        assert!(
            auth.to_ascii_lowercase().contains("bearer test-key"),
            "health check must send bearer auth: {auth}"
        );
    }

    #[tokio::test]
    async fn health_check_401_names_env_var() {
        let (addr, _seen) = spawn_stub(vec![Stub::Status(401)]).await;
        let client = Client::new(&cfg_for(addr)).unwrap();
        let err = client.health_check().await.unwrap_err().to_string();
        assert!(
            err.contains("PIFORGE_API_KEY"),
            "401 must name the env var: {err}"
        );
        assert!(
            err.contains(&addr.to_string()),
            "401 must name base_url: {err}"
        );
    }

    #[tokio::test]
    async fn chat_retries_on_429_then_succeeds() {
        let (addr, seen) = spawn_stub(vec![Stub::Status(429), Stub::Status(200)]).await;
        let client = Client::build(&cfg_for(addr), fast_retry(), Duration::from_secs(5)).unwrap();
        let resp = client
            .chat(&chat_req())
            .await
            .expect("should succeed after retry");
        assert_eq!(resp.content, "ok");
        assert_eq!(resp.prompt_tokens, 1);
        assert_eq!(resp.completion, 2);
        assert_eq!(seen.lock().unwrap().len(), 2, "first 429, then 200");
    }

    #[tokio::test]
    async fn chat_retries_on_5xx_then_succeeds() {
        let (addr, seen) = spawn_stub(vec![
            Stub::Status(503),
            Stub::Status(500),
            Stub::Status(200),
        ])
        .await;
        let client = Client::build(&cfg_for(addr), fast_retry(), Duration::from_secs(5)).unwrap();
        client
            .chat(&chat_req())
            .await
            .expect("should succeed after two retries");
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn chat_fails_bounded_after_max_retries_on_500() {
        // max_retries=3 → 4 total attempts, all 500 → fails after the 4th.
        let (addr, seen) = spawn_stub(vec![Stub::Status(500); 8]).await;
        let client = Client::build(&cfg_for(addr), fast_retry(), Duration::from_secs(5)).unwrap();
        let err = client.chat(&chat_req()).await.unwrap_err().to_string();
        assert!(err.contains("500"), "error should report 500: {err}");
        assert_eq!(
            seen.lock().unwrap().len(),
            4,
            "exactly max_retries+1 attempts, no hang"
        );
    }

    #[tokio::test]
    async fn chat_fails_fast_on_401_and_names_env_var() {
        let (addr, seen) = spawn_stub(vec![Stub::Status(401)]).await;
        let client = Client::build(&cfg_for(addr), fast_retry(), Duration::from_secs(5)).unwrap();
        let err = client.chat(&chat_req()).await.unwrap_err().to_string();
        assert!(
            err.contains("PIFORGE_API_KEY"),
            "401 must name env var: {err}"
        );
        assert_eq!(seen.lock().unwrap().len(), 1, "401 must NOT be retried");
    }

    #[tokio::test]
    async fn chat_fails_fast_on_400() {
        let (addr, seen) = spawn_stub(vec![Stub::Status(400)]).await;
        let client = Client::build(&cfg_for(addr), fast_retry(), Duration::from_secs(5)).unwrap();
        let err = client.chat(&chat_req()).await.unwrap_err().to_string();
        assert!(err.contains("400"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1, "400 must NOT be retried");
    }

    #[tokio::test]
    async fn chat_fails_fast_on_404() {
        let (addr, seen) = spawn_stub(vec![Stub::Status(404)]).await;
        let client = Client::build(&cfg_for(addr), fast_retry(), Duration::from_secs(5)).unwrap();
        let err = client.chat(&chat_req()).await.unwrap_err().to_string();
        assert!(err.contains("404"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1, "404 must NOT be retried");
    }

    #[tokio::test]
    async fn chat_fails_fast_on_403_names_env_var() {
        let (addr, seen) = spawn_stub(vec![Stub::Status(403)]).await;
        let client = Client::build(&cfg_for(addr), fast_retry(), Duration::from_secs(5)).unwrap();
        let err = client.chat(&chat_req()).await.unwrap_err().to_string();
        assert!(
            err.contains("PIFORGE_API_KEY"),
            "403 must name env var: {err}"
        );
        assert_eq!(seen.lock().unwrap().len(), 1, "403 must NOT be retried");
    }

    #[tokio::test]
    async fn chat_error_scrubs_key_value_from_body() {
        // A malicious provider echoes the bearer token in its 401 body. The
        // surfaced error must name the env var but never contain the key VALUE.
        let (addr, _seen) = spawn_stub(vec![Stub::Body {
            code: 401,
            body: r#"{"echo":"Authorization: Bearer test-key leaked!"}"#.to_string(),
        }])
        .await;
        let client = Client::build(&cfg_for(addr), fast_retry(), Duration::from_secs(5)).unwrap();
        let err = client.chat(&chat_req()).await.unwrap_err().to_string();
        assert!(err.contains("PIFORGE_API_KEY"), "must name env var: {err}");
        assert!(
            !err.contains("test-key"),
            "key VALUE must be scrubbed from the error: {err}"
        );
    }

    #[tokio::test]
    async fn chat_retries_on_connection_reset_then_succeeds() {
        // First attempt: RST (transient transport). Retry: 200. Both attempts
        // are recorded, proving the reset was retried (not just that it succeeded).
        let (addr, seen) = spawn_stub(vec![Stub::Reset, Stub::Status(200)]).await;
        let client = Client::build(&cfg_for(addr), fast_retry(), Duration::from_secs(5)).unwrap();
        let resp = client
            .chat(&chat_req())
            .await
            .expect("reset should be retried");
        assert_eq!(resp.content, "ok");
        let g = seen.lock().unwrap();
        assert_eq!(g.len(), 2, "reset attempt + successful 200");
        assert_eq!(g[0].request_line, "<reset>");
    }

    #[tokio::test]
    async fn chat_retries_on_request_timeout_then_succeeds() {
        // First attempt: hang → client timeout (is_timeout). Retry: 200.
        let (addr, seen) = spawn_stub(vec![Stub::Hang, Stub::Status(200)]).await;
        let client =
            Client::build(&cfg_for(addr), fast_retry(), Duration::from_millis(80)).unwrap();
        let resp = client
            .chat(&chat_req())
            .await
            .expect("timeout should be retried");
        assert_eq!(resp.content, "ok");
        let g = seen.lock().unwrap();
        assert_eq!(g.len(), 2, "hang attempt + successful 200");
        assert_eq!(g[0].request_line, "<hang>");
    }

    #[tokio::test]
    async fn chat_fails_bounded_after_max_retries_on_reset() {
        // A "down provider" modeled as repeated resets (transient transport) must
        // fail after max_retries+1 attempts, not hang or retry forever.
        let (addr, seen) = spawn_stub(vec![Stub::Reset; 8]).await;
        let client = Client::build(&cfg_for(addr), fast_retry(), Duration::from_secs(5)).unwrap();
        let err = client.chat(&chat_req()).await.unwrap_err().to_string();
        assert!(
            err.contains("request failed"),
            "transport error surfaced: {err}"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            4,
            "exactly max_retries+1 attempts, no hang/retry-forever"
        );
    }
}
