//! The HIL tool surface — a common trait every tool implements. The agent loop
//! is agnostic to whether a tool is the real hardware implementation
//! (`hw` feature, Linux-only) or a simulated one (driven by eval fixtures).
//!
//! A `ToolResult` is returned as JSON the model parses as the tool-call output.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use crate::provider::{FunctionDefinition, Tool as ProvTool};

/// A tool the agent can call. Implementations live in `sim` (cross-platform)
/// and `hil_hw` (Linux-only, behind the `hw` feature).
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> ProvTool {
        ProvTool {
            kind: "function".into(),
            function: FunctionDefinition {
                name: self.name().into(),
                description: self.description().into(),
                parameters: self.parameters(),
            },
        }
    }
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    async fn execute(&self, args: &Value) -> ToolResult;
}

/// The structured result a tool returns. Serialized to JSON for the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

impl ToolResult {
    pub fn ok(tool: &str, value: Value) -> Self {
        Self {
            tool: tool.into(),
            ok: true,
            value: Some(value),
            unit: None,
            error: None,
            notice: None,
        }
    }
    pub fn ok_unit(tool: &str, value: Value, unit: &str) -> Self {
        Self {
            tool: tool.into(),
            ok: true,
            value: Some(value),
            unit: Some(unit.into()),
            error: None,
            notice: None,
        }
    }
    pub fn err(tool: &str, msg: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            ok: false,
            value: None,
            unit: None,
            error: Some(msg.into()),
            notice: None,
        }
    }
    pub fn to_json_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{\"ok\":false}".into())
    }
}

/// The safety gate a tool consults before Class I (physical) ops.
/// Implemented by [`crate::broker::Gate`].
pub trait Gate: Send + Sync {
    fn allow(&self, physical: &str, detail: Value) -> Result<(), String>;
}

/// A tool-box convenience.
pub type ToolVec = Vec<Arc<dyn Tool>>;

/// Unified telemetry snapshot value (sim and hw). `telemetry_known` tracks
/// throttled-state only — temp `"N/A"` must not flip it. `dmesg_tail` is
/// omitted when empty.
pub fn telemetry_snapshot(
    throttled: Option<u64>,
    cpu_temp: &str,
    dmesg_tail: Option<&str>,
) -> Value {
    let known = throttled.is_some();
    let (throttled_raw, under_voltage) = match throttled {
        Some(val) => (
            serde_json::json!(format!("0x{val:x}")),
            serde_json::json!(val & (1 << 0) != 0 || val & (1 << 16) != 0),
        ),
        None => (Value::Null, Value::Null),
    };
    let mut out = serde_json::json!({
        "throttled_raw": throttled_raw,
        "under_voltage": under_voltage,
        "cpu_temp": cpu_temp,
        "telemetry_known": known,
    });
    if let Some(d) = dmesg_tail {
        if !d.is_empty() {
            out["dmesg_tail"] = serde_json::json!(d);
        }
    }
    out
}
