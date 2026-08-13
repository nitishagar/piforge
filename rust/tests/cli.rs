//! Binary CLI: `--task` is not clap-required; `--tool` skips the LLM.
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_piforge"))
}

#[test]
fn missing_task_bails_without_marking_flag_required() {
    let out = bin()
        .args(["--config", "none"])
        .output()
        .expect("run piforge");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no --task given"),
        "expected I19 runtime bail, got {err}"
    );
    assert!(
        !err.to_lowercase().contains("required argument"),
        "clap must not mark --task required: {err}"
    );
}

// `--tool` talks to sim tools and must not require llama-server. The hw
// feature resolves `/dev/i2c-N` at startup, so this path is sim-only.
#[cfg(not(feature = "hw"))]
#[test]
fn tool_inventory_skips_llm() {
    let out = bin()
        .args(["--config", "none", "--tool", "hardware_inventory"])
        .output()
        .expect("run piforge --tool");
    assert!(
        out.status.success(),
        "stderr={} stdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"tool\":\"hardware_inventory\"") && stdout.contains("\"ok\":true"),
        "expected inventory JSON, got {stdout}"
    );
}
