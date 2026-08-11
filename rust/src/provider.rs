//! Llama-server (OpenAI-compatible) client. Talks to a local llama-server's
//! `/v1/chat/completions` endpoint with `tools` + `tool_choice`, and reports
//! token usage including the cache telemetry (`prompt_tokens_details.cached_tokens`)
//! that verifies the prefix-cache premise.
use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};

use crate::config::ServerConfig;

/// The chat-completion client.
pub struct Client {
    http: HttpClient,
    base_url: String,
    api_key: String,
    max_tokens: u32,
    temperature: f32,
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
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(300)) // a slow Pi can take minutes/turn
            .build()?;
        Ok(Self {
            http,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
            metrics: parking_lot::Mutex::new(Metrics::default()),
        })
    }

    /// Health check: hit /models to confirm the server is up + serving OpenAI shape.
    pub async fn health_check(&self) -> Result<()> {
        let url = format!("{}/models", self.base_url);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "server health check failed (is llama-server running at {}?): HTTP {}",
                self.base_url,
                resp.status()
            ));
        }
        Ok(())
    }

    /// Non-streaming chat completion. Returns content + tool calls + telemetry.
    pub async fn chat(&self, req: &ChatRequest) -> Result<ChatResponse> {
        let body = ChatCompletionRequest {
            // llama-server ignores model; uses its loaded GGUF.
            model: "piforge".into(),
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
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("chat completion: HTTP {status}: {text}"));
        }
        let cc: ChatCompletionResponse = resp.json().await?;

        let choice = cc.choices.first().ok_or_else(|| anyhow!("empty choices"))?;
        let mut out = ChatResponse {
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

    pub fn metrics(&self) -> Metrics {
        *self.metrics.lock()
    }
}

/// Input to a chat completion.
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
