//! Configuration: TOML file + env overrides + defaults + validation.
//! Direct port of the Go config so the same piforge.toml works for both.
use std::env;
use std::path::Path;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: ModelConfig,
    pub server: ServerConfig,
    pub agent: AgentConfig,
    pub hardware: HardwareConfig,
    pub safety: SafetyConfig,
    pub eval: EvalConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: ModelConfig::default(),
            server: ServerConfig::default(),
            agent: AgentConfig::default(),
            hardware: HardwareConfig::default(),
            safety: SafetyConfig::default(),
            eval: EvalConfig::default(),
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub base_url: String,
    pub api_key: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080/v1".into(),
            api_key: "dummy".into(),
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
    /// RP1 register max is 12mA; do not raise.
    pub per_pin_max_current_ma: u32,
    /// Conservative guideline (no RP1 spec published).
    pub rail_budget_ma: u32,
    pub stop_on_under_voltage: bool,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            arm_mode: "confirm".into(),
            per_pin_max_current_ma: 12,
            rail_budget_ma: 50,
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
    Ok(cfg)
}

fn apply_env(cfg: &mut Config) {
    if let Ok(v) = env::var("PIFORGE_BASE_URL") {
        if !v.is_empty() {
            cfg.server.base_url = v;
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

/// Validate checks for obvious errors. Mirrors the Go Validate().
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
        if self.safety.per_pin_max_current_ma > 12 {
            return Err(anyhow!(
                "safety.per_pin_max_current_ma {} exceeds RP1 max of 12mA",
                self.safety.per_pin_max_current_ma
            ));
        }
        if self.safety.arm_mode == "auto"
            && env::var("PIFORGE_ALLOW_AUTO_ARM").as_deref() != Ok("1")
        {
            return Err(anyhow!(
                "safety.arm_mode=auto requires PIFORGE_ALLOW_AUTO_ARM=1 (DANGEROUS)"
            ));
        }
        Ok(())
    }
}

/// Convenience: true if a config file exists at the given path.
pub fn exists(path: &str) -> bool {
    !path.is_empty() && path != "none" && Path::new(path).exists()
}
