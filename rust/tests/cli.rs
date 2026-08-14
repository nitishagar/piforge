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

// Non-TTY stdin (pipes/CI): a Class I `gpio set` must be DENIED by the
// confirmer — fail-closed end-to-end through the real binary. Sim telemetry
// is always known (sim's ThrottledReader), so the confirm gate is the one
// that fires, deterministically. A deadline-bounded wait: a lock inversion
// here (gate under the state lock) hangs, and the test must FAIL, not hang.
#[cfg(not(feature = "hw"))]
#[test]
fn tool_gpio_set_denied_with_piped_stdin() {
    use std::time::{Duration, Instant};
    let mut child = bin()
        .args([
            "--config",
            "none",
            "--tool",
            "gpio",
            "--args",
            r#"{"action":"set","pin":17,"value":1}"#,
        ])
        // Explicit null stdin: the deny must be deterministic regardless of
        // whether the test harness itself runs on a TTY.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn piforge --tool gpio set");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("poll piforge") {
            Some(st) => break st,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                panic!("piforge --tool gpio set did not terminate in 30s (deadlock?)");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let out = child.wait_with_output().expect("collect output");
    assert!(!status.success(), "unattended Class I set must not succeed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("DENIED by user (op=gpio_set risk=I/level1)"),
        "expected broker refusal in tool result, got stdout={stdout} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}
