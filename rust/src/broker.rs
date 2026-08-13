//! The safety gate between the agent and physical hardware. Implements the
//! operation classification (R/B/I) and the under-voltage STOP signal.
//!
//! Red-team fixes baked in:
//!   - The under-voltage check runs FIRST, even in auto-arm mode (it must not
//!     be bypassed).
//!   - Arming is scoped + time-limited (30s window per op+pin); never a global
//!     permanent toggle.
//!   - The confirm callback is invoked outside the lock so a deliberating
//!     human doesn't block other Class I ops or their under-voltage rechecks.
use std::collections::HashMap;
use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use serde_json::Value;

use crate::config::SafetyConfig;
use crate::hil::Gate;

/// Under-voltage tri-state. `known=false` is unknown telemetry, not healthy 0x0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UvReading {
    pub known: bool,
    pub now: bool,
    pub since: bool,
}

/// Reads the under-voltage state. Implemented by telemetry tools (real + sim).
pub trait ThrottledReader: Send + Sync {
    fn under_voltage_active(&self) -> UvReading;
}

/// The broker gate. `confirm` returns true to allow a Class I op.
pub struct Broker {
    cfg: SafetyConfig,
    tel: Option<Arc<dyn ThrottledReader>>,
    confirm: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    armed: Mutex<HashMap<String, Instant>>,
}

impl Broker {
    pub fn new(
        cfg: SafetyConfig,
        tel: Option<Arc<dyn ThrottledReader>>,
        confirm: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Self {
        Self {
            cfg,
            tel,
            confirm,
            armed: Mutex::new(HashMap::new()),
        }
    }

    /// Convenience: always-deny confirmer (for eval/unattended runs).
    pub fn always_deny() -> Arc<dyn Fn(&str) -> bool + Send + Sync> {
        Arc::new(|_| false)
    }

    /// Accept trimmed `y`/`Y`/`yes`/`YES` (Go StdinConfirmer).
    pub fn confirm_line(line: &str) -> bool {
        matches!(line.trim(), "y" | "Y" | "yes" | "YES")
    }

    /// Interactive Class I confirmer. TTY: prompt on stderr, read a line.
    /// Non-TTY: deny without reading (fail-closed for CI/pipes).
    pub fn stdin_confirmer() -> Arc<dyn Fn(&str) -> bool + Send + Sync> {
        Arc::new(|prompt| {
            if !io::stdin().is_terminal() {
                return false;
            }
            eprint!("{prompt}");
            let _ = io::stderr().flush();
            let mut line = String::new();
            match io::stdin().lock().read_line(&mut line) {
                Ok(_) => Self::confirm_line(&line),
                Err(_) => false,
            }
        })
    }
}

impl Gate for Broker {
    fn allow(&self, physical: &str, detail: Value) -> Result<(), String> {
        // 1. Under-voltage STOP — ALWAYS first, even in auto mode.
        // Deny if telemetry is missing, unknown, or UV now/since. Confirm stays
        // outside the arm lock below.
        if self.cfg.stop_on_under_voltage {
            match &self.tel {
                None => {
                    return Err(
                        "REFUSED: under-voltage telemetry unavailable — Class I denied".into(),
                    );
                }
                Some(tel) => {
                    let uv = tel.under_voltage_active();
                    if !uv.known {
                        return Err("REFUSED: under-voltage state unknown — Class I denied".into());
                    }
                    if uv.now || uv.since {
                        return Err(format!(
                            "REFUSED: under-voltage active (now={} since_boot={}) \
                             — adding load risks brownout/SD corruption; upgrade PSU and clear before retrying",
                            uv.now, uv.since
                        ));
                    }
                }
            }
        }

        // 2. Auto-arm short-circuit (eval/batch; config.validate gates the env var).
        if self.cfg.arm_mode == "auto" {
            return Ok(());
        }

        // 3. Scoped arm cache.
        let key = arm_key(physical, &detail);
        {
            let mut armed = self.armed.lock();
            if let Some(exp) = armed.get(&key) {
                if Instant::now() < *exp {
                    return Ok(()); // armed within the window
                }
            }
        }

        // 4. Build the prompt, then ask the human WITHOUT holding the lock.
        let risk = risk_tier(physical);
        let prompt = format_prompt(physical, &detail, risk);
        let ok = (self.confirm)(&prompt);
        if !ok {
            return Err(format!("DENIED by user (op={physical} risk={risk})"));
        }

        // 5. Grant a scoped, 30s arm; prune expired entries.
        {
            let mut armed = self.armed.lock();
            armed.insert(key, Instant::now() + Duration::from_secs(30));
            armed.retain(|_, exp| Instant::now() < *exp);
        }
        Ok(())
    }
}

fn arm_key(physical: &str, detail: &Value) -> String {
    if let Some(pin) = detail.get("pin") {
        format!("{physical}:pin={pin}")
    } else if let Some(addr) = detail.get("address") {
        format!("{physical}:addr={addr}")
    } else {
        physical.into()
    }
}

fn risk_tier(physical: &str) -> &'static str {
    match physical {
        "gpio_set" => "I/level1",
        "pwm_start" | "motor_drive" | "relay_drive" => "I/level2-mechanical",
        "i2c_write" => "I/peripheral",
        "flash_write" | "eeprom_write" => "I/irreversible",
        _ => "I",
    }
}

fn format_prompt(physical: &str, detail: &Value, risk: &str) -> String {
    let mut bits = String::new();
    for k in ["pin", "value", "address", "register", "frequency"] {
        if let Some(v) = detail.get(k) {
            bits.push_str(&format!(" {k}={v}"));
        }
    }
    format!("[PiForge {risk}]{bits} approve? (y/N): ")
}
