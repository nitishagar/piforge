//! Configuration: TOML file + env overrides + defaults + validation.
//! Existence of `hardware.i2c_bus` is checked at hw resolve, not here (macOS
//! has no `/dev/i2c-N`).
use std::env;
use std::path::Path;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub model: ModelConfig,
    pub server: ServerConfig,
    pub agent: AgentConfig,
    pub hardware: HardwareConfig,
    pub safety: SafetyConfig,
    pub eval: EvalConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub path: String,
    pub quant: String,
    /// Context window. On Pi 5 8GB with Q4 + Q8 KV, 8192 is realistic.
    pub context: usize,
    /// "off" (non-thinking, interactive) | "on" (thinking, batch only).
    pub thinking_mode: String,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            path: String::new(),
            quant: "Q4_K_M".into(),
            context: 8192,
            thinking_mode: "off".into(),
        }
    }
}

/// Default `server.base_url` (the local llama-server). Also the sentinel that
/// tells [`apply_provider`] the user didn't override it, so a provider preset
/// may fill it in.
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080/v1";
/// Default `server.model` (a local llama-server ignores it). Same sentinel
/// purpose for provider presets.
const DEFAULT_MODEL: &str = "piforge";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub base_url: String,
    pub api_key: String,
    /// Model id sent in the chat-completion request body. A local llama-server
    /// ignores this (it serves its loaded GGUF); a cloud provider requires the
    /// real id (e.g. `glm-4.6`, `gpt-4o`). Default keeps the prior local behavior.
    pub model: String,
    /// Optional provider preset name (e.g. `"zai-coding"`, `"openai"`). When set,
    /// `base_url` + `model` are filled from a built-in registry (explicit toml or
    /// env values still win). Empty => manual `base_url` + `model` mode.
    pub provider: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            api_key: "dummy".into(),
            model: DEFAULT_MODEL.into(),
            provider: String::new(),
            max_tokens: 1024,
            temperature: 0.2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub max_turns: u32,
    pub telemetry_preload: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 12,
            telemetry_preload: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HardwareConfig {
    pub board: String,
    /// Pi 5 40-pin header chip; empty => auto-discover (gpiochip4).
    pub gpiochip: String,
    pub i2c_bus: String,
}

impl Default for HardwareConfig {
    fn default() -> Self {
        Self {
            board: String::new(),
            gpiochip: String::new(),
            i2c_bus: "/dev/i2c-1".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    /// "confirm" (default) | "auto" (DANGEROUS; needs PIFORGE_ALLOW_AUTO_ARM=1).
    pub arm_mode: String,
    pub stop_on_under_voltage: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            arm_mode: "confirm".into(),
            stop_on_under_voltage: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvalConfig {
    pub cases_dir: String,
    pub models: Vec<String>,
    pub pass_rate_threshold: f64,
    pub hallucination_threshold: f64,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            cases_dir: "eval/cases".into(),
            models: vec![],
            pass_rate_threshold: 0.55,
            hallucination_threshold: 0.10,
        }
    }
}

/// Load piforge.toml from `path` ("" or "none" => defaults + env only),
/// then apply env-var overrides.
pub fn load(path: &str) -> Result<Config> {
    let mut cfg = Config::default();
    if !path.is_empty() && path != "none" {
        let text = std::fs::read_to_string(path).map_err(|e| anyhow!("read {path}: {e}"))?;
        cfg = toml::from_str(&text).map_err(|e| anyhow!("decode {path}: {e}"))?;
    }
    apply_env(&mut cfg);
    apply_provider(&mut cfg)?;
    Ok(cfg)
}

fn apply_env(cfg: &mut Config) {
    if let Ok(v) = env::var("PIFORGE_BASE_URL") {
        if !v.is_empty() {
            cfg.server.base_url = v;
        }
    }
    // Model id (server.model); env wins over toml and provider presets.
    if let Ok(v) = env::var("PIFORGE_MODEL") {
        if !v.is_empty() {
            cfg.server.model = v;
        }
    }
    // Cloud API key. Env wins over toml (applied AFTER the toml parse in load()),
    // so a real key can be supplied without ever writing it to a committed file.
    // Unset → the toml/default "dummy" remains, which local llama-server ignores.
    if let Ok(v) = env::var("PIFORGE_API_KEY") {
        if !v.is_empty() {
            cfg.server.api_key = v;
        }
    }
    // Provider preset name (e.g. "zai-coding"). Resolved into base_url + model by
    // apply_provider() after env; env wins over the toml value.
    if let Ok(v) = env::var("PIFORGE_PROVIDER") {
        if !v.is_empty() {
            cfg.server.provider = v;
        }
    }
    if let Ok(v) = env::var("PIFORGE_MODEL_PATH") {
        if !v.is_empty() {
            cfg.model.path = v;
        }
    }
    if let Ok(v) = env::var("PIFORGE_CONTEXT") {
        if let Ok(n) = v.parse::<usize>() {
            if n > 0 {
                cfg.model.context = n;
            }
        }
    }
    if let Ok(v) = env::var("PIFORGE_MAX_TOKENS") {
        if let Ok(n) = v.parse::<u32>() {
            if n > 0 {
                cfg.server.max_tokens = n;
            }
        }
    }
    if let Ok(v) = env::var("PIFORGE_GPIO_CHIP") {
        if !v.is_empty() {
            cfg.hardware.gpiochip = v;
        }
    }
    if let Ok(v) = env::var("PIFORGE_I2C_BUS") {
        if !v.is_empty() {
            cfg.hardware.i2c_bus = v;
        }
    }
    if let Ok(v) = env::var("PIFORGE_ARM_MODE") {
        if v == "auto" || v == "confirm" {
            cfg.safety.arm_mode = v;
        }
    }
}

/// A built-in provider preset: the endpoint + a recommended model.
#[derive(Clone, Copy)]
struct ProviderPreset {
    base_url: &'static str,
    model: &'static str,
}

/// Known provider presets. The HTTP client stays generic (one OpenAI-compatible
/// client); this table is pure DATA mapping a name → endpoint + model, so a user
/// sets `provider = "<name>"` + `PIFORGE_API_KEY` and the endpoint follows from
/// the provider. To keep it generic, no provider gets bespoke request/auth code.
///
/// z.ai note: it exposes two OpenAI-compat endpoints billed differently —
/// `/api/paas/v4` (pay-as-you-go) vs `/api/coding/paas/v4` (GLM Coding Plan
/// subscription). A Coding-Plan key on `/api/paas/v4` returns 1113; use the
/// matching endpoint.
fn known_providers() -> &'static [(&'static str, ProviderPreset)] {
    &[
        (
            "zai",
            ProviderPreset {
                base_url: "https://api.z.ai/api/paas/v4",
                model: "glm-4.6",
            },
        ),
        (
            "zai-paas",
            ProviderPreset {
                base_url: "https://api.z.ai/api/paas/v4",
                model: "glm-4.6",
            },
        ),
        (
            "zai-coding",
            ProviderPreset {
                base_url: "https://api.z.ai/api/coding/paas/v4",
                model: "glm-4.6",
            },
        ),
        (
            "openai",
            ProviderPreset {
                base_url: "https://api.openai.com/v1",
                model: "gpt-4o",
            },
        ),
        (
            "kimi",
            ProviderPreset {
                base_url: "https://api.moonshot.cn/v1",
                model: "moonshot-v1-32k",
            },
        ),
        (
            "openrouter",
            ProviderPreset {
                base_url: "https://openrouter.ai/api/v1",
                model: "anthropic/claude-3.5-sonnet",
            },
        ),
    ]
}

fn lookup_provider(name: &str) -> Option<ProviderPreset> {
    let lower = name.to_ascii_lowercase();
    known_providers()
        .iter()
        .find(|(n, _)| *n == lower)
        .map(|(_, p)| *p)
}

/// Resolve a provider preset: fill `base_url` + `model` from the registry when
/// the user left them at the default (explicit toml/env values still win).
/// Errors on an unknown provider name, listing the known ones.
fn apply_provider(cfg: &mut Config) -> Result<()> {
    if cfg.server.provider.is_empty() {
        return Ok(());
    }
    let preset = match lookup_provider(&cfg.server.provider) {
        Some(p) => p,
        None => {
            let known: Vec<&str> = known_providers().iter().map(|(n, _)| *n).collect();
            return Err(anyhow!(
                "unknown server.provider {:?}; known providers: {}",
                cfg.server.provider,
                known.join(", ")
            ));
        }
    };
    if cfg.server.base_url == DEFAULT_BASE_URL {
        cfg.server.base_url = preset.base_url.into();
    }
    if cfg.server.model == DEFAULT_MODEL {
        cfg.server.model = preset.model.into();
    }
    Ok(())
}

/// Validate checks for obvious errors.
impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.server.base_url.is_empty() {
            return Err(anyhow!("server.base_url must be set"));
        }
        if !self.server.base_url.starts_with("http") {
            return Err(anyhow!("server.base_url must be an http(s) URL"));
        }
        if self.model.context < 512 {
            return Err(anyhow!(
                "model.context {} too small (min 512)",
                self.model.context
            ));
        }
        if self.safety.arm_mode == "auto"
            && env::var("PIFORGE_ALLOW_AUTO_ARM").as_deref() != Ok("1")
        {
            return Err(anyhow!(
                "safety.arm_mode=auto requires PIFORGE_ALLOW_AUTO_ARM=1 (DANGEROUS)"
            ));
        }
        // Trailing decimal index only. Existence is hw-resolve, not validate (macOS).
        if !self.hardware.i2c_bus.is_empty() {
            let digits: String = self
                .hardware
                .i2c_bus
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if digits.is_empty() {
                return Err(anyhow!(
                    "hardware.i2c_bus {:?} must contain a trailing decimal bus index \
                     (existence is checked at hw resolve, not here — macOS has no /dev/i2c-N)",
                    self.hardware.i2c_bus
                ));
            }
        }
        Ok(())
    }
}

/// Convenience: true if a config file exists at the given path.
pub fn exists(path: &str) -> bool {
    !path.is_empty() && path != "none" && Path::new(path).exists()
}
