//! Sim-side tool-box byte-stability + result-key pinning (the CI-runnable half
//! of cross-build parity).
//!
//! The sim and hw builds should expose identical `name()`/`parameters()` schemas
//! and result-JSON field names: the system prompt + tool schemas are a
//! byte-stable KV-cache prefix, and the eval scorer matches on result field
//! names. The CI runner builds the sim crate only, so this file pins the **sim**
//! contract against committed snapshots — it guards the half of that contract
//! which is verifiable off-hardware, and enforces the `duration_ms` scope-parity
//! fix.
//!
//! - **Layer 1 (`prefix_byte_stable`):** serialize every sim tool's `schema()` +
//!   `SYSTEM_PROMPT` and assert byte-identical to `tests/snapshots/sim_prefix.json`.
//!   Catches sim-side `name()`/`parameters()`/prompt drift. (`schema()` cannot
//!   see result-JSON field names — hence Layer 2.)
//! - **Layer 2 (`sim_result_keys_stable`):** execute each sim tool and assert the
//!   sorted top-level + nested key-set of its result value matches
//!   `tests/snapshots/sim_result_keys.json`. Catches sim-side result-field drift
//!   (e.g. dropping `duration_ms` from the scope result).
//!
//! What this file does NOT do (and cannot, off-hardware): execute hw tools.
//! Hw `schema()` + `SYSTEM_PROMPT` are pinned separately by `tests/hw_prefix.rs`
//! (`cfg(feature="hw")`). Result key-sets were unified this change (inventory,
//! telemetry, gpio); remaining sim↔hw execute differences (live `/dev` vs sim
//! state) are not in `schema()`.
//!
//! The snapshots are self-bootstrapping: the first run creates them and fails
//! ("re-run to verify"); subsequent runs assert stability. To re-pin after an
//! intentional change, delete the snapshot file and re-run.
use piforge::agent::SYSTEM_PROMPT;
use piforge::hil::Tool;
use piforge::provider::Tool as ProvTool;
use piforge::sim::{self, Setup};
use serde::Serialize;
use serde_json::{json, Value};

/// Build the 6-tool sim box (same set + order as the bin's sim helper / hw build).
fn sim_tools() -> Vec<std::sync::Arc<dyn Tool>> {
    // One gpio pin so `gpio get` + `scope` return a value (not an error).
    let mut pins = std::collections::HashMap::new();
    pins.insert(0i32, "in".to_string());
    let st = sim::State::from_setup(Setup {
        gpio_pins: pins,
        ..Setup::default()
    });
    vec![
        sim::InventoryTool::new(st.clone()),
        sim::TelemetryTool::new(st.clone()),
        sim::I2CTool::new(st.clone()),
        sim::GPIOTool::new(st.clone(), None),
        sim::ScopeTool::new(st.clone()),
        sim::CodeEditTool::new("."),
    ]
}

fn snapshot_path(name: &str) -> String {
    format!("{}/tests/snapshots/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Assert `actual == pinned snapshot`, creating the snapshot on first run.
fn assert_or_pin(name: &str, actual: &str) {
    let path = snapshot_path(name);
    if let Ok(expected) = std::fs::read_to_string(&path) {
        assert_eq!(
            actual, expected,
            "{name} drifted (prefix/result-key contract). \
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

#[test]
fn prefix_byte_stable() {
    let tools = sim_tools();
    let schemas: Vec<ProvTool> = tools.iter().map(|t| t.schema()).collect();
    let prefix = Prefix {
        system_prompt: SYSTEM_PROMPT,
        tools: schemas,
    };
    // to_string_pretty is deterministic: struct fields serialize in declaration
    // order and serde_json::Map (BTreeMap by default) sorts object keys, so the
    // output is stable across machines.
    let actual = format!("{}\n", serde_json::to_string_pretty(&prefix).unwrap());
    assert_or_pin("sim_prefix.json", &actual);
}

/// Recursively collect the sorted key-set of a JSON value: every object key
/// path (top-level + nested) plus `[]` for array-element object shapes.
fn key_set(v: &Value) -> String {
    fn rec(v: &Value, out: &mut Vec<String>, prefix: &str) {
        match v {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for k in keys {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    out.push(path.clone());
                    rec(&map[k], out, &path);
                }
            }
            Value::Array(arr) => {
                // Element shape: use the first element (sim returns deterministic
                // single-element arrays, e.g. scope's events[]).
                if let Some(first) = arr.first() {
                    rec(first, out, &format!("{prefix}[]"));
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    rec(v, &mut out, "");
    out.join(",")
}

#[tokio::test]
async fn sim_result_keys_stable() {
    let tools = sim_tools();
    // The 5 hardware-variant tools (sim vs hw can diverge). `edit_file` is shared
    // code in both builds so it cannot drift and is skipped (also avoids a
    // file-write side effect in the test).
    let cases: &[(&str, Value)] = &[
        ("hardware_inventory", json!({})),
        ("telemetry", json!({"action":"snapshot"})),
        ("i2c", json!({"action":"scan"})),
        ("gpio", json!({"action":"get","pin":0})),
        ("scope", json!({"pin":0,"duration":500})),
    ];
    let mut lines = Vec::new();
    for (name, args) in cases {
        let tool = tools
            .iter()
            .find(|t| t.name() == *name)
            .unwrap_or_else(|| panic!("tool {name} present"));
        let res = tool.execute(args).await;
        let value = res.value.clone().unwrap_or(Value::Null);
        assert!(res.ok, "`{name}` returned an error: {:?}", res.error);
        lines.push(format!("{name}: {}", key_set(&value)));
    }
    let actual = format!("{}\n", lines.join("\n"));
    assert_or_pin("sim_result_keys.json", &actual);
}
