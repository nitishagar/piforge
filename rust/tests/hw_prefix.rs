//! Hw-side prefix snapshot (`schema()` + `SYSTEM_PROMPT`). Construct tools with
//! a dummy chip/bus — `schema()` needs no `/dev`.
#![cfg(feature = "hw")]

use piforge::agent::SYSTEM_PROMPT;
use piforge::hil::Tool;
use piforge::hil_hw::{GpioTool, I2cAdapter, I2cTool, InventoryTool, ScopeTool, TelemetryTool};
use piforge::provider::Tool as ProvTool;
use piforge::sim::CodeEditTool;
use serde::Serialize;

fn snapshot_path(name: &str) -> String {
    format!("{}/tests/snapshots/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn assert_or_pin(name: &str, actual: &str) {
    let path = snapshot_path(name);
    if let Ok(expected) = std::fs::read_to_string(&path) {
        assert_eq!(
            actual, expected,
            "{name} drifted (hw prefix contract). \
             If intentional, delete {path} and re-run to re-pin."
        );
    } else {
        std::fs::create_dir_all(format!("{}/tests/snapshots", env!("CARGO_MANIFEST_DIR")))
            .expect("create snapshots dir");
        std::fs::write(&path, actual).expect("write snapshot");
        panic!(
            "{name} snapshot created at {path}. Re-run to verify stability; \
             commit the snapshot file."
        );
    }
}

#[derive(Serialize)]
struct Prefix {
    system_prompt: &'static str,
    tools: Vec<ProvTool>,
}

fn hw_tools() -> Vec<std::sync::Arc<dyn Tool>> {
    let bus = I2cAdapter::new("/dev/i2c-1");
    vec![
        InventoryTool::new(bus.clone()),
        TelemetryTool::new(),
        I2cTool::new(bus),
        GpioTool::new("gpiochip0", None),
        ScopeTool::new("gpiochip0"),
        CodeEditTool::new("."),
    ]
}

#[test]
fn hw_prefix_byte_stable() {
    let tools = hw_tools();
    let schemas: Vec<ProvTool> = tools.iter().map(|t| t.schema()).collect();
    let prefix = Prefix {
        system_prompt: SYSTEM_PROMPT,
        tools: schemas,
    };
    let actual = format!("{}\n", serde_json::to_string_pretty(&prefix).unwrap());
    assert_or_pin("hw_prefix.json", &actual);
}
