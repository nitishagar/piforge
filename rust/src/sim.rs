//! Simulated HIL tools driven by eval Case fixtures. Implements the same
//! [`hil::Tool`] interface as the real hardware tools, so the agent loop is
//! identical between live and eval runs.
use parking_lot::Mutex;
use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::broker::ThrottledReader;
use crate::hil::{Gate, Tool, ToolResult};

/// Simulated board state, loaded from a Case fixture's Setup.
pub struct State {
    inner: Mutex<StateInner>,
}

struct StateInner {
    board: String,
    i2c: Option<I2CBus>,
    gpio: HashMap<i32, Pin>,
    dmesg_tail: Vec<String>,
    throttled: u64,
    files: HashMap<String, String>,
}

#[derive(Default)]
struct I2CBus {
    devices: HashMap<i32, I2CDevice>,
    /// "all" => every address responds (shorted-bus fault).
    scan_pattern: String,
}

struct I2CDevice {
    chip: String,
}

struct Pin {
    value: i32,
}

/// A sim fixture (mirrors eval::Case::Setup).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Setup {
    #[serde(default)]
    pub board: String,
    #[serde(default)]
    pub i2c_devices: HashMap<i32, String>,
    #[serde(default)]
    pub scan_pattern: String,
    #[serde(default)]
    pub gpio_pins: HashMap<i32, String>,
    #[serde(default)]
    pub files: HashMap<String, String>,
    #[serde(default)]
    pub dmesg_tail: Vec<String>,
    #[serde(default)]
    pub throttled: String,
}

impl State {
    pub fn from_setup(s: Setup) -> Arc<Self> {
        // SAFETY note: Arc not used here but kept consistent with shared usage.
        let mut i2c_devices = HashMap::new();
        for (addr, chip) in &s.i2c_devices {
            if *addr == 0 {
                continue;
            }
            i2c_devices.insert(*addr, I2CDevice { chip: chip.clone() });
        }
        let i2c = if s.i2c_devices.is_empty() && s.scan_pattern.is_empty() {
            None
        } else {
            Some(I2CBus {
                devices: i2c_devices,
                scan_pattern: s.scan_pattern.clone(),
            })
        };
        let mut gpio = HashMap::new();
        for (pin, _mode) in s.gpio_pins {
            gpio.insert(pin, Pin { value: 0 });
        }
        Arc::new(Self {
            inner: Mutex::new(StateInner {
                board: s.board,
                i2c,
                gpio,
                dmesg_tail: s.dmesg_tail,
                throttled: parse_hex(&s.throttled),
                files: s.files,
            }),
        })
    }
}

// Note: sim tools hold an Arc<State> and may be cloned into the tool-box.
use std::sync::Arc;

// ---- telemetry adapter for the broker gate ----
impl ThrottledReader for State {
    fn under_voltage_active(&self) -> (bool, bool) {
        let s = self.inner.lock();
        (s.throttled & (1 << 0) != 0, s.throttled & (1 << 16) != 0)
    }
}

// ---- inventory tool ----
pub struct InventoryTool {
    st: Arc<State>,
}
impl InventoryTool {
    pub fn new(st: Arc<State>) -> Arc<Self> {
        Arc::new(Self { st })
    }
}

#[async_trait]
impl Tool for InventoryTool {
    fn name(&self) -> &str {
        "hardware_inventory"
    }
    fn description(&self) -> &str {
        "List detected hardware: I2C devices, GPIO pins, board model. Call first."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{}})
    }
    async fn execute(&self, _args: &Value) -> ToolResult {
        let s = self.st.inner.lock();
        let mut inv = json!({"board": s.board});
        if let Some(bus) = &s.i2c {
            let addrs: Vec<String> = bus.devices.keys().map(|a| format!("0x{a:02x}")).collect();
            inv["i2c_devices"] = json!(addrs);
        }
        if !s.gpio.is_empty() {
            let pins: Vec<i32> = s.gpio.keys().copied().collect();
            inv["gpio_pins"] = json!(pins);
        }
        ToolResult::ok("hardware_inventory", inv)
    }
}

// ---- telemetry tool ----
pub struct TelemetryTool {
    st: Arc<State>,
}
impl TelemetryTool {
    pub fn new(st: Arc<State>) -> Arc<Self> {
        Arc::new(Self { st })
    }
}

#[async_trait]
impl Tool for TelemetryTool {
    fn name(&self) -> &str {
        "telemetry"
    }
    fn description(&self) -> &str {
        "Read Pi health telemetry: temp, volts, throttled bitmask, dmesg tail."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["snapshot","throttled","temp"]}},"required":["action"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("snapshot");
        let s = self.st.inner.lock();
        match action {
            "temp" => ToolResult::ok_unit("telemetry", json!({"cpu_temp":"temp=48.5'C"}), "degC"),
            "throttled" => {
                let val = s.throttled;
                let decoded = json!({
                    "under_voltage_now": val & (1<<0) != 0,
                    "currently_throttled_now": val & (1<<2) != 0,
                    "under_voltage_since_boot": val & (1<<16) != 0,
                    "throttled_since_boot": val & (1<<18) != 0,
                });
                let notice = if val & (1 << 0) != 0 || val & (1 << 16) != 0 {
                    Some("UNDERVOLTAGE detected — STOP before adding load.".into())
                } else {
                    None
                };
                ToolResult {
                    tool: "telemetry".into(),
                    ok: true,
                    value: Some(json!({"raw":format!("0x{val:x}"),"decoded":decoded})),
                    unit: None,
                    error: None,
                    notice,
                }
            }
            _ => {
                let mut out =
                    json!({"cpu_temp":"temp=48.5'C","core_volts":"volt=1.0V","board":s.board});
                let uv = s.throttled & (1 << 0) != 0 || s.throttled & (1 << 16) != 0;
                if uv {
                    out["notice"] = json!("UNDERVOLTAGE detected — STOP before adding load.");
                }
                if !s.dmesg_tail.is_empty() {
                    out["dmesg_tail"] = json!(s.dmesg_tail.join("\n"));
                }
                ToolResult::ok("telemetry", out)
            }
        }
    }
}

// ---- i2c tool ----
pub struct I2CTool {
    st: Arc<State>,
}
impl I2CTool {
    pub fn new(st: Arc<State>) -> Arc<Self> {
        Arc::new(Self { st })
    }
}

#[async_trait]
impl Tool for I2CTool {
    fn name(&self) -> &str {
        "i2c"
    }
    fn description(&self) -> &str {
        "I2C scan/detect/read. Returns structured values with units."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["scan","read","detect"]},"address":{"type":"integer"},"register":{"type":"integer"},"length":{"type":"integer"}},"required":["action"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("scan");
        let addr = args.get("address").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let s = self.st.inner.lock();
        match action {
            "scan" => match &s.i2c {
                None => ToolResult::ok("i2c", json!({"devices":[],"count":0})),
                Some(bus) if bus.scan_pattern == "all" => {
                    let hex: Vec<String> = (0x08..=0x77).map(|a| format!("0x{a:02x}")).collect();
                    ToolResult { tool:"i2c".into(), ok:true, value:Some(json!({"devices":hex,"count":hex.len()})), unit:Some("7-bit addr".into()), error:None, notice:Some("many addresses responded — likely SDA/SCL shorted to power; STOP and check wiring".into()) }
                }
                Some(bus) => {
                    let found: Vec<String> =
                        bus.devices.keys().map(|a| format!("0x{a:02x}")).collect();
                    let notice = if found.is_empty() {
                        Some("no devices — check dtparam=i2c_arm=on, wiring, pull-ups".into())
                    } else {
                        None
                    };
                    ToolResult {
                        tool: "i2c".into(),
                        ok: true,
                        value: Some(json!({"devices":found,"count":found.len()})),
                        unit: Some("7-bit addr".into()),
                        error: None,
                        notice,
                    }
                }
            },
            "detect" => {
                if let Some(bus) = &s.i2c {
                    if bus.devices.contains_key(&addr) {
                        return ToolResult::ok(
                            "i2c",
                            json!({"address":format!("0x{addr:02x}"),"present":true}),
                        );
                    }
                }
                ToolResult::err("i2c", format!("no device at 0x{addr:02x}"))
            }
            "read" => {
                if let Some(bus) = &s.i2c {
                    if let Some(dev) = bus.devices.get(&addr) {
                        let reg = args.get("register").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                        let n = args.get("length").and_then(|v| v.as_i64()).unwrap_or(1) as usize;
                        let zeros: Vec<u8> = vec![0; n];
                        let raw_hex: String = zeros.iter().map(|b| format!("{b:02x}")).collect();
                        let mut out = json!({"address":format!("0x{addr:02x}"),"register":format!("0x{reg:02x}"),"raw_hex":raw_hex,"raw_dec":zeros});
                        if !dev.chip.is_empty() {
                            out["chip"] = json!(dev.chip);
                        }
                        return ToolResult::ok_unit(
                            "i2c",
                            out,
                            "bytes (see datasheet for scaling)",
                        );
                    }
                }
                ToolResult::err("i2c", format!("no device at 0x{addr:02x}"))
            }
            _ => ToolResult::err("i2c", format!("unknown action {action}")),
        }
    }
}

// ---- gpio tool ----
pub struct GPIOTool {
    st: Arc<State>,
    gate: Option<Arc<dyn Gate>>,
}
impl GPIOTool {
    pub fn new(st: Arc<State>, gate: Option<Arc<dyn Gate>>) -> Arc<Self> {
        Arc::new(Self { st, gate })
    }
}

#[async_trait]
impl Tool for GPIOTool {
    fn name(&self) -> &str {
        "gpio"
    }
    fn description(&self) -> &str {
        "GPIO get (safe) / set (Class I: physical, requires arming)."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"action":{"type":"string","enum":["get","set"]},"pin":{"type":"integer"},"value":{"type":"integer","enum":[0,1]}},"required":["action","pin"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("get");
        let pin = args.get("pin").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let mut s = self.st.inner.lock();
        match action {
            "get" => match s.gpio.get(&pin) {
                Some(p) => {
                    ToolResult::ok_unit("gpio", json!({"pin":pin,"value":p.value}), "level(0|1)")
                }
                None => ToolResult::err("gpio", format!("pin {pin} not in profile")),
            },
            "set" => {
                let value = args.get("value").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                if value != 0 && value != 1 {
                    return ToolResult::err("gpio", "value must be 0 or 1");
                }
                if let Some(g) = &self.gate {
                    if let Err(e) = g.allow("gpio_set", json!({"pin":pin,"value":value})) {
                        return ToolResult::err("gpio", format!("DENIED by safety gate: {e}"));
                    }
                }
                if !s.gpio.contains_key(&pin) {
                    return ToolResult::err("gpio", format!("pin {pin} not in profile"));
                }
                s.gpio.get_mut(&pin).unwrap().value = value;
                ToolResult::ok_unit(
                    "gpio",
                    json!({"pin":pin,"value":value,"driven":true}),
                    "level(0|1)",
                )
            }
            _ => ToolResult::err("gpio", format!("unknown action {action}")),
        }
    }
}

// ---- scope tool (sim: returns a single static edge) ----
pub struct ScopeTool {
    st: Arc<State>,
}
impl ScopeTool {
    pub fn new(st: Arc<State>) -> Arc<Self> {
        Arc::new(Self { st })
    }
}

#[async_trait]
impl Tool for ScopeTool {
    fn name(&self) -> &str {
        "scope"
    }
    fn description(&self) -> &str {
        "Capture GPIO edge events over a window (Class R)."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"pin":{"type":"integer"},"duration":{"type":"number"}},"required":["pin","duration"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let pin = args.get("pin").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        // `duration_ms` mirrors the hw scope's result field: the two builds must
        // expose the same result-JSON field names, so a hw-only rename can't
        // desync the KV-cache prefix or the eval scorer.
        let dur_ms = args.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let s = self.st.inner.lock();
        match s.gpio.get(&pin) {
            Some(p) => {
                let edge = if p.value == 1 { "rising" } else { "falling" };
                ToolResult::ok(
                    "scope",
                    json!({"pin":pin,"duration_ms":dur_ms,"edges":1,"rate_hz":0,"events":[{"t_us":0,"edge":edge}]}),
                )
            }
            None => ToolResult::err("scope", format!("pin {pin} not in profile")),
        }
    }
}

// ---- code edit tool (whole-file, sandboxed to a workspace root) ----
pub struct CodeEditTool {
    root: String,
    edits: Mutex<u32>,
}
impl CodeEditTool {
    pub fn new(root: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            root: root.into(),
            edits: Mutex::new(0),
        })
    }
    pub fn edits(&self) -> u32 {
        *self.edits.lock()
    }
}

#[async_trait]
impl Tool for CodeEditTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Write full new file contents (whole-file edit, not patch). Path relative to workspace root."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]})
    }
    async fn execute(&self, args: &Value) -> ToolResult {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return ToolResult::err("edit_file", "path required"),
        };
        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
        match safe_join(&self.root, path) {
            Ok(abs) => {
                if let Some(parent) = abs.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        return ToolResult::err("edit_file", format!("mkdir: {e}"));
                    }
                }
                if let Err(e) = std::fs::write(&abs, content) {
                    return ToolResult::err("edit_file", format!("write: {e}"));
                }
                *self.edits.lock() += 1;
                ToolResult::ok("edit_file", json!({"path":path,"bytes":content.len()}))
            }
            Err(e) => ToolResult::err("edit_file", e),
        }
    }
}

/// Resolve a relative path under root, rejecting traversal + symlink escapes.
/// Evaluates symlinks; uses a path-separator boundary for the ".." check.
fn safe_join(root: &str, rel: &str) -> Result<std::path::PathBuf, String> {
    use std::path::{Component, PathBuf};
    if std::path::Path::new(rel).is_absolute() {
        return Err("path must be relative to workspace root".into());
    }
    let clean_root = std::fs::canonicalize(root).map_err(|e| format!("resolve root: {e}"))?;
    // Build the joined path from normalized components (reject .. escapes lexically).
    let mut joined = clean_root.clone();
    for comp in std::path::Path::new(rel).components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(c) => joined.push(c),
            Component::ParentDir => return Err(format!("path {rel:?} escapes workspace root")),
            _ => return Err("path contains unsupported component".into()),
        }
    }
    // Evaluate symlinks on the parent (the file itself may not exist yet).
    let parent = joined.parent().unwrap_or_else(|| std::path::Path::new("/"));
    let clean_parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.into());
    let abs = clean_parent.join(joined.file_name().unwrap_or_default());
    // Final containment check: relative path from root must not start with "..".
    match abs.strip_prefix(&clean_root) {
        Ok(_) => Ok(abs),
        Err(_) => Err(format!("path {rel:?} escapes workspace root")),
    }
}

/// Parse a throttled hex string (handles "0x10000", "throttled=0x10000", "").
fn parse_hex(s: &str) -> u64 {
    let s = s.trim();
    let s = if let Some(eq) = s.find('=') {
        &s[eq + 1..]
    } else {
        s
    };
    let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    u64::from_str_radix(s, 16).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_handles_shapes() {
        assert_eq!(parse_hex("0x0"), 0);
        assert_eq!(parse_hex("0x10000"), 0x10000);
        assert_eq!(parse_hex("throttled=0x1"), 1);
        assert_eq!(parse_hex(""), 0);
    }
}
