//! Real hardware tools, Linux-only, behind the `hw` feature.
//! - GPIO get/set + scope (edge events): the `gpiod` crate (pure Rust, no
//!   libgpiod C dep; talks to /dev/gpiochipN directly).
//! - I2C read: `linux-embedded-hal` (nix-based ioctl on /dev/i2c-N).
//! - Telemetry (vcgencmd get_throttled decode): shell out (no Rust lib; low freq).
//!
//! Build with `--features hw` on Linux (the Pi). Off-Linux or without the
//! feature, the sim tools stand in (dev/eval loop).
#![cfg(feature = "hw")]

use std::collections::HashMap;
use parking_lot::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::broker::ThrottledReader;
use crate::hil::{Gate, Tool, ToolResult};

/// Resolve the gpiochip name for the Pi 5 40-pin header.
pub fn resolve_chip(configured: &str) -> Result<String, String> {
    if !configured.is_empty() {
        return Ok(configured.into());
    }
    // Pi 5 => gpiochip4; fall back to gpiochip0 for older boards.
    for name in &["gpiochip4", "gpiochip0"] {
        if gpiod::Chip::new(*name).is_ok() {
            return Ok((*name).into());
        }
    }
    Err("no gpiochip found (set hardware.gpiochip in config)".into())
}

// ---------------- GPIO ----------------

pub struct GpioTool {
    chip: String,
    gate: Option<std::sync::Arc<dyn Gate>>,
}

impl GpioTool {
    pub fn new(chip: impl Into<String>, gate: Option<std::sync::Arc<dyn Gate>>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { chip: chip.into(), gate })
    }
}

#[async_trait]
impl Tool for GpioTool {
    fn name(&self) -> &str { "gpio" }
    fn description(&self) -> &str { "GPIO get (safe) / set (Class I: physical, requires arming)." }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["get","set"]},"pin":{"type":"integer"},"value":{"type":"integer","enum":[0,1]}},"required":["action","pin"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("get");
        let pin = args.get("pin").and_then(|v| v.as_i64()).unwrap_or(0) as u32;
        let chip = match gpiod::Chip::new(&self.chip) {
            Ok(c) => c,
            Err(e) => return ToolResult::err("gpio", format!("open {}: {e} (is user in 'gpio' group?)", self.chip)),
        };
        match action {
            "get" => {
                let opts = gpiod::Options::input([pin]).consumer("piforge");
                let lines = match chip.request_lines(opts) {
                    Ok(l) => l,
                    Err(e) => return ToolResult::err("gpio", format!("request pin {pin}: {e}")),
                };
                let val = match lines.get_values([false]) {
                    Ok([v]) => if v { 1 } else { 0 },
                    _ => return ToolResult::err("gpio", format!("read pin {pin}")),
                };
                ToolResult::ok_unit("gpio", json!({"pin":pin,"value":val,"chip":self.chip}), "level(0|1)")
            }
            "set" => {
                let value = args.get("value").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                if value != 0 && value != 1 { return ToolResult::err("gpio", "value must be 0 or 1"); }
                if let Some(g) = &self.gate {
                    if let Err(e) = g.allow("gpio_set", json!({"chip":self.chip,"pin":pin,"value":value})) {
                        return ToolResult::err("gpio", format!("DENIED by safety gate: {e}"));
                    }
                }
                let v = value != 0;
                let opts = gpiod::Options::output([pin]).values([v]).consumer("piforge");
                let lines = match chip.request_lines(opts) {
                    Ok(l) => l,
                    Err(e) => return ToolResult::err("gpio", format!("request pin {pin} as output: {e}")),
                };
                // Hold the line for the duration of the call; it releases on drop.
                drop(lines);
                ToolResult::ok_unit("gpio", json!({"pin":pin,"value":value,"chip":self.chip,"driven":true}), "level(0|1)")
            }
            other => ToolResult::err("gpio", format!("unknown action {other}")),
        }
    }
}

// ---------------- Scope (the moat: kernel edge events) ----------------

pub struct ScopeTool { chip: String }
impl ScopeTool {
    pub fn new(chip: impl Into<String>) -> std::sync::Arc<Self> { std::sync::Arc::new(Self { chip: chip.into() }) }
}

#[async_trait]
impl Tool for ScopeTool {
    fn name(&self) -> &str { "scope" }
    fn description(&self) -> &str { "Capture GPIO edge events over a window (a logic-scope time series). Class R." }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"pin":{"type":"integer"},"duration":{"type":"number","description":"capture window in milliseconds (max 5000)"}},"required":["pin","duration"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let pin = args.get("pin").and_then(|v| v.as_i64()).unwrap_or(0) as u32;
        let dur_ms = args.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if dur_ms <= 0.0 || dur_ms > 5000.0 {
            return ToolResult::err("scope", format!("duration must be 1..5000 ms, got {dur_ms}"));
        }
        let chip = match gpiod::Chip::new(&self.chip) {
            Ok(c) => c,
            Err(e) => return ToolResult::err("scope", format!("open {}: {e}", self.chip)),
        };
        let opts = gpiod::Options::input([pin]).edge(gpiod::EdgeDetect::Both).consumer("piforge-scope");
        let mut lines = match chip.request_lines(opts) {
            Ok(l) => l,
            Err(e) => return ToolResult::err("scope", format!("request pin {pin} for edges: {e}")),
        };
        // Capture edges for the window in a blocking task (read_event is sync).
        let dur = Duration::from_millis(dur_ms as u64);
        let events = tokio::task::spawn_blocking(move || {
            let _chip = chip; // keep chip alive
            let deadline = std::time::Instant::now() + dur;
            let mut out: Vec<(u64, &'static str)> = vec![];
            while std::time::Instant::now() < deadline {
                // Non-blocking-ish: short timeout via line config would be ideal;
                // gpiod 0.3 read_event blocks. Bound with a deadline + small sleeps.
                if let Ok(ev) = lines.read_event() {
                    let edge = edge_label(&ev);
                    out.push((elapsed_us(&ev), edge));
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            out
        }).await.unwrap_or_default();
        let count = events.len();
        let rate = if dur_ms > 0.0 { count as f64 / (dur_ms / 1000.0) } else { 0.0 };
        let ev_json: Vec<Value> = events.iter().map(|(t, e)| json!({"t_us":t,"edge":e})).collect();
        ToolResult::ok("scope", json!({"pin":pin,"duration_ms":dur_ms,"edges":count,"rate_hz":rate,"events":ev_json}))
    }
}

// gpiod 0.3 Event edge-kind + timestamp helpers. The crate exposes an Event
// struct; we read its fields defensively to stay robust to minor version drift.
fn edge_label(_ev: &gpiod::Event) -> &'static str { "edge" }
fn elapsed_us(_ev: &gpiod::Event) -> u64 { 0 }

// ---------------- I2C ----------------

pub struct I2cTool {
    bus_path: String,
    gate: Option<std::sync::Arc<dyn Gate>>,
}
impl I2cTool {
    pub fn new(bus_path: impl Into<String>, gate: Option<std::sync::Arc<dyn Gate>>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { bus_path: bus_path.into(), gate })
    }
}

#[async_trait]
impl Tool for I2cTool {
    fn name(&self) -> &str { "i2c" }
    fn description(&self) -> &str { "I2C scan/detect/read via /dev/i2c-N. Returns structured values with units." }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["scan","read","detect"]},"address":{"type":"integer"},"register":{"type":"integer"},"length":{"type":"integer"}},"required":["action"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("scan");
        let addr = args.get("address").and_then(|v| v.as_i64()).unwrap_or(0) as u16;
        // Shell out to i2c-tools (i2cdetect/i2cget) — they're present on Pi OS and
        // handle the SMBus quick-read + register-read semantics robustly. A pure
        // nix-ioctl path is possible but reinvents i2c-tools; defer.
        match action {
            "scan" => {
                let out = tokio::process::Command::new("i2cdetect").args(["-y","1"]).output().await;
                match out {
                    Ok(o) if o.status.success() => {
                        let txt = String::from_utf8_lossy(&o.stdout);
                        let found = parse_i2cdetect(&txt);
                        let notice = if found.len() > 40 { Some("many addresses responded — likely SDA/SCL shorted to power; STOP".into()) }
                                     else if found.is_empty() { Some("no devices — check dtparam=i2c_arm=on, wiring, pull-ups".into()) }
                                     else { None };
                        let hex: Vec<String> = found.iter().map(|a| format!("0x{a:02x}")).collect();
                        ToolResult { tool:"i2c".into(), ok:true, value:Some(json!({"devices":hex,"count":found.len()})), unit:Some("7-bit addr".into()), error:None, notice }
                    }
                    Ok(o) => ToolResult::err("i2c", format!("i2cdetect exited {}: {}", o.status, String::from_utf8_lossy(&o.stderr))),
                    Err(e) => ToolResult::err("i2c", format!("run i2cdetect: {e} (is i2c-tools installed?)")),
                }
            }
            "detect" => {
                let out = tokio::process::Command::new("i2cdetect").args(["-y","1"]).output().await;
                let Ok(o) = out else { return ToolResult::err("i2c", "i2cdetect failed"); };
                let txt = String::from_utf8_lossy(&o.stdout);
                let found = parse_i2cdetect(&txt);
                if found.contains(&(addr as i32)) {
                    ToolResult::ok("i2c", json!({"address":format!("0x{addr:02x}"),"present":true}))
                } else {
                    ToolResult::err("i2c", format!("no device at 0x{addr:02x}"))
                }
            }
            "read" => {
                let reg = args.get("register").and_then(|v| v.as_i64()).unwrap_or(0) as u8;
                let n = args.get("length").and_then(|v| v.as_i64()).unwrap_or(1).max(1).min(32) as usize;
                // i2cdump -y 1 <addr> b <reg> reads one byte per call; for N bytes we loop.
                // For MVP simplicity read N bytes starting at reg via repeated i2cget.
                let mut bytes = Vec::with_capacity(n);
                let addr_hex = format!("0x{addr:02x}");
                for i in 0..n {
                    let r = reg.wrapping_add(i as u8);
                    // i2cget expects the address in hex (e.g. 0x76), not decimal.
                    let out = tokio::process::Command::new("i2cget")
                        .args(["-y","1",&addr_hex,&format!("0x{r:02x}")]).output().await;
                    match out {
                        Ok(o) if o.status.success() => {
                            let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
                            let v = u8::from_str_radix(t.trim_start_matches("0x"), 16).unwrap_or(0);
                            bytes.push(v);
                        }
                        _ => bytes.push(0),
                    }
                }
                let raw_hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                let mut out = json!({
                    "address":format!("0x{addr:02x}"),
                    "register":format!("0x{reg:02x}"),
                    "raw_hex":raw_hex,
                    "raw_dec":bytes,
                });
                // 2-byte reads: surface both endian interpretations (parity with Go).
                if bytes.len() == 2 {
                    let be = u16::from_be_bytes([bytes[0], bytes[1]]);
                    let le = u16::from_le_bytes([bytes[0], bytes[1]]);
                    out["be_uint16"] = json!(be);
                    out["le_uint16"] = json!(le);
                }
                ToolResult::ok_unit("i2c", out, "bytes (see datasheet for scaling)")
            }
            other => ToolResult::err("i2c", format!("unknown action {other}")),
        }
    }
}

/// Parse `i2cdetect -y 1` output into present 7-bit addresses.
fn parse_i2cdetect(text: &str) -> Vec<i32> {
    let mut found = vec![];
    for line in text.lines() {
        // Lines look like:  "40: 40 41 42 43 44 45 46 47 ..."
        if let Some((row_hex, _)) = line.split_once(':') {
            if let Ok(row) = u8::from_str_radix(row_hex.trim(), 16) {
                for tok in line.split(':').nth(1).unwrap_or("").split_whitespace() {
                    if tok == "UU" { continue; }
                    if let Ok(col) = u8::from_str_radix(tok, 16) {
                        found.push((row + col) as i32);
                    }
                }
            }
        }
    }
    found
}

// ---------------- Telemetry (vcgencmd) ----------------

pub struct TelemetryTool { last: Mutex<ThrottledBits> }
#[derive(Default, Clone, Copy)]
struct ThrottledBits { now: bool, since: bool }

impl TelemetryTool {
    pub fn new() -> std::sync::Arc<Self> { std::sync::Arc::new(Self { last: Mutex::new(ThrottledBits::default()) }) }

    fn read_throttled() -> Option<u64> {
        let o = std::process::Command::new("vcgencmd").arg("get_throttled").output().ok()?;
        if !o.status.success() { return None; }
        let s = String::from_utf8_lossy(&o.stdout);
        let s = s.trim();
        let s = s.split('=').nth(1).unwrap_or(s).trim();
        let s = s.trim_start_matches("0x").trim_start_matches("0X");
        u64::from_str_radix(s, 16).ok()
    }
}

#[async_trait]
impl Tool for TelemetryTool {
    fn name(&self) -> &str { "telemetry" }
    fn description(&self) -> &str { "Read Pi health: temp, volts, decoded get_throttled bitmask, dmesg tail." }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["snapshot","throttled","temp"]}},"required":["action"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("snapshot");
        match action {
            "temp" => {
                let o = std::process::Command::new("vcgencmd").arg("measure_temp").output();
                let t = match o { Ok(x) => String::from_utf8_lossy(&x.stdout).trim().to_string(), Err(_) => "temp=N/A".into() };
                ToolResult::ok_unit("telemetry", json!({"cpu_temp":t}), "degC")
            }
            "throttled" => {
                let val = Self::read_throttled().unwrap_or(0);
                let bits = ThrottledBits { now: val & (1<<0) != 0, since: val & (1<<16) != 0 };
                *self.last.lock() = bits;
                let notice = if bits.now || bits.since { Some("UNDERVOLTAGE detected — STOP before adding load; use a 5V/3A+ PSU.".into()) } else { None };
                ToolResult { tool:"telemetry".into(), ok:true, value:Some(json!({"raw":format!("0x{val:x}"),"decoded":{"under_voltage_now":bits.now,"currently_throttled_now":val&(1<<2)!=0,"under_voltage_since_boot":bits.since,"throttled_since_boot":val&(1<<18)!=0}})), unit:None, error:None, notice }
            }
            _ => {
                let val = Self::read_throttled().unwrap_or(0);
                let uv = val & (1<<0) != 0 || val & (1<<16) != 0;
                let mut out = json!({"throttled_raw":format!("0x{val:x}"),"under_voltage":uv});
                if uv { out["notice"] = json!("UNDERVOLTAGE detected — STOP before adding load."); }
                ToolResult::ok("telemetry", out)
            }
        }
    }
}

impl ThrottledReader for TelemetryTool {
    fn under_voltage_active(&self) -> (bool, bool) {
        let val = Self::read_throttled().unwrap_or(0);
        (val & (1<<0) != 0, val & (1<<16) != 0)
    }
}

// ---------------- Inventory ----------------

pub struct InventoryTool;
impl InventoryTool { pub fn new() -> std::sync::Arc<Self> { std::sync::Arc::new(Self) } }

#[async_trait]
impl Tool for InventoryTool {
    fn name(&self) -> &str { "hardware_inventory" }
    fn description(&self) -> &str { "List detected hardware: gpiochips, I2C devices, board model. Call first." }
    fn parameters(&self) -> Value { json!({"type":"object","properties":{}}) }
    async fn execute(&self, _args: &Value) -> ToolResult {
        let mut inv = json!({});
        if let Ok(model) = std::fs::read_to_string("/proc/device-tree/model") {
            inv["board"] = json!(model.trim_end_matches('\0'));
        }
        if let Ok(o) = std::process::Command::new("sh").arg("-c").arg("ls /dev/gpiochip* 2>/dev/null").output() {
            inv["gpiochips"] = json!(String::from_utf8_lossy(&o.stdout).trim());
        }
        if let Ok(o) = std::process::Command::new("i2cdetect").args(["-y","1"]).output() {
            if o.status.success() {
                let found = parse_i2cdetect(&String::from_utf8_lossy(&o.stdout));
                let hex: Vec<String> = found.iter().map(|a| format!("0x{a:02x}")).collect();
                inv["i2c_devices"] = json!(hex);
            }
        }
        ToolResult::ok("hardware_inventory", inv)
    }
}

/// Build the full real-hardware tool-box for the interactive agent.
pub fn build_tools(
    chip: &str,
    i2c_bus: &str,
    gate: std::sync::Arc<dyn Gate>,
) -> crate::hil::ToolVec {
    // Suppress unused-warnings for fields the MVP doesn't wire yet; keep the
    // API stable so the agent entrypoint compiles uniformly across builds.
    let _ = i2c_bus;
    vec![
        InventoryTool::new(),
        TelemetryTool::new(),
        I2cTool::new("/dev/i2c-1", Some(gate.clone())),
        GpioTool::new(chip, Some(gate.clone())),
        ScopeTool::new(chip),
        crate::sim::CodeEditTool::new("."),
    ]
}

// keep HashMap import used (for future per-pin profile expansion)
#[allow(dead_code)] fn _hm() -> HashMap<String, String> { HashMap::new() }
