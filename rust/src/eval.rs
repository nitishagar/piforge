//! The capability eval — the gate that must pass before the local-appliance
//! product is built. Cases run headless: the agent's tool calls are served by
//! the sim package from each Case fixture, edits land in a per-case temp dir,
//! and a mock provider lets it all run in CI without a llama-server.
//!
//! Decision rule: ≥55% fix-rate AND <10% register/pin hallucination → BUILD_LOCAL.
use std::io::Write;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::agent::{Agent, LlmClient, MockProvider, MockTurn, RunError, TraceEvent};
use crate::broker::{Broker, ThrottledReader};
use crate::hil::ToolVec;
use crate::provider::ToolCall;
use crate::sim::{self, CodeEditTool, Setup, State};

/// One eval fixture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    pub id: String,
    #[serde(default)]
    pub archetype: String,
    pub symptom: String,
    pub setup: Setup,
    pub gold: Gold,
    #[serde(default)]
    pub hallucinated: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Gold {
    #[serde(default)]
    pub diagnosis: String,
    #[serde(default)]
    pub fix_applies: String,
    #[serde(default)]
    pub fix_must_contain: Vec<String>,
    #[serde(default)]
    pub fix_must_not_have: Vec<String>,
    #[serde(default)]
    pub is_hardware_fault: bool,
}

/// Why a case errored — the attribution split. A turn-budget exhaustion is
/// model non-convergence, NOT a harness error: it counts in `non_converged`,
/// while the other kinds count in `errored` (and everything still fails the
/// case, preserving the `pass_rate` denominator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseError {
    ProviderFailure,
    WorkspaceError,
    Interrupted,
    TurnBudgetExhausted,
}

impl CaseError {
    pub fn is_harness_error(&self) -> bool {
        !matches!(self, CaseError::TurnBudgetExhausted)
    }
}

/// Per-case verdict.
#[derive(Debug, Clone, Default)]
pub struct Verdict {
    pub case_id: String,
    pub pass_: bool,
    pub partial: bool,
    pub hallucination: bool,
    pub error_kind: Option<CaseError>,
    pub duration_sec: f64,
    pub turns: u32,
    pub cache_hit_rate: f64,
    pub notes: String,
}

/// The runner. Holds either a real client (Arc<dyn LlmClient>) or is built
/// in mock mode for CI.
pub struct Runner {
    client: Option<std::sync::Arc<dyn LlmClient>>,
    mock: Option<std::sync::Arc<MockProvider>>,
    max_turns: u32,
    preload: bool,
    trace_dir: Option<PathBuf>,
    /// Cases whose trace degraded (write failure) — reported at end of run,
    /// never a run failure.
    degraded: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
}

impl Runner {
    pub fn new(client: std::sync::Arc<dyn LlmClient>, max_turns: u32, preload: bool) -> Self {
        Self {
            client: Some(client),
            mock: None,
            max_turns,
            preload,
            trace_dir: None,
            degraded: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }
    pub fn new_mock(max_turns: u32, preload: bool) -> (Self, std::sync::Arc<MockProvider>) {
        let mock = std::sync::Arc::new(MockProvider::new());
        (
            Self {
                client: None,
                mock: Some(mock.clone()),
                max_turns,
                preload,
                trace_dir: None,
                degraded: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
            },
            mock,
        )
    }

    /// Write one JSONL trace per case under `dir/<case-id>.jsonl` (the binary
    /// creates a per-run subdirectory). Traces are local run artifacts.
    pub fn with_trace_dir(mut self, dir: PathBuf) -> Self {
        self.trace_dir = Some(dir);
        self
    }

    /// Case ids whose trace degraded during the run (write failures).
    pub fn degraded_traces(&self) -> Vec<String> {
        self.degraded.lock().clone()
    }

    /// Run all *.json cases in `cases_dir`, invoking `progress` per verdict.
    /// Exclusive and sequential: one case at a time (per-case workspaces are
    /// pid+case-id keyed), one case's agent per run. A SIGINT classifies the
    /// in-flight case as `Interrupted` and the remaining cases still run.
    pub async fn run_all<F>(&self, cases_dir: &str, mut progress: F) -> Result<Vec<Verdict>>
    where
        F: FnMut(&Verdict),
    {
        let mut entries: Vec<_> = std::fs::read_dir(cases_dir)
            .map_err(|e| anyhow!("read cases dir {cases_dir}: {e}"))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("read cases dir {cases_dir}: {e}"))?
            .into_iter()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();
        entries.sort_by_key(|e| e.path());
        let mut verdicts = vec![];
        for e in entries {
            let path = e.path();
            let case: Case = serde_json::from_str(
                &std::fs::read_to_string(&path)
                    .map_err(|err| anyhow!("read {}: {err}", path.display()))?,
            )
            .map_err(|err| anyhow!("parse {}: {err}", path.display()))?;
            let v = self.run_case(&case).await;
            progress(&v);
            verdicts.push(v);
        }
        Ok(verdicts)
    }

    async fn run_case(&self, c: &Case) -> Verdict {
        let start = std::time::Instant::now();
        let mut v = Verdict {
            case_id: c.id.clone(),
            ..Default::default()
        };

        // Per-case temp workspace seeded with the fixture's files.
        let workspace = match temp_workspace(&c.id, &c.setup.files) {
            Ok(w) => w,
            Err(e) => {
                v.error_kind = Some(CaseError::WorkspaceError);
                v.notes = format!("error: {e}");
                v.duration_sec = start.elapsed().as_secs_f64();
                return v;
            }
        };

        // Sim state + tools.
        let st = State::from_setup(c.setup.clone());
        let st_reader: std::sync::Arc<dyn ThrottledReader> = st.clone();
        let gate = std::sync::Arc::new(Broker::new(
            crate::config::SafetyConfig {
                arm_mode: "auto".into(), // eval/batch; main validates the env guard
                stop_on_under_voltage: true,
            },
            Some(st_reader),
            Broker::always_deny(),
        ));
        let edit_tool = CodeEditTool::new(workspace.path.to_string_lossy().into_owned());
        let tools: ToolVec = vec![
            sim::InventoryTool::new(st.clone()),
            sim::TelemetryTool::new(st.clone()),
            sim::I2CTool::new(st.clone()),
            sim::GPIOTool::new(st.clone(), Some(gate)),
            sim::ScopeTool::new(st.clone()),
            edit_tool.clone(),
        ];

        // Per-case trace sink: one JSONL file, stamped with the case id on
        // every line. Write failures degrade the trace (flagged at end of
        // run) and never fail the case — the trace must not change behavior.
        let trace_sink: Option<crate::agent::TraceSink> =
            self.trace_dir
                .as_ref()
                .map(|dir| -> crate::agent::TraceSink {
                    let path = dir.join(format!("{}.jsonl", sanitize_case_id(&c.id)));
                    let file = match std::fs::File::create(&path) {
                        Ok(f) => f,
                        Err(_) => {
                            self.degraded.lock().push(c.id.clone());
                            return std::sync::Arc::new(|_: &TraceEvent| {});
                        }
                    };
                    let writer = std::sync::Arc::new(parking_lot::Mutex::new(Some(file)));
                    let degraded = self.degraded.clone();
                    let case_id = c.id.clone();
                    std::sync::Arc::new(move |ev: &TraceEvent| {
                        let mut guard = writer.lock();
                        let Some(file) = guard.as_mut() else { return };
                        let mut line = match serde_json::to_value(ev) {
                            Ok(v) => v,
                            Err(_) => return,
                        };
                        line["case_id"] = serde_json::json!(case_id);
                        if writeln!(file, "{line}").is_err() {
                            *guard = None;
                            drop(guard);
                            degraded.lock().push(case_id.clone());
                        }
                    })
                });

        let res = match &self.client {
            Some(client) => {
                let mut agent = Agent::new(client.clone(), tools, self.max_turns, self.preload);
                if let Some(sink) = &trace_sink {
                    agent = agent.with_trace(sink.clone());
                }
                agent.run(&c.symptom, |_| ()).await
            }
            None => {
                let mock = self.mock.clone().unwrap();
                mock.load(script(&c.id)).await;
                let mut agent = Agent::new(
                    mock as std::sync::Arc<dyn LlmClient>,
                    tools,
                    self.max_turns,
                    self.preload,
                );
                if let Some(sink) = &trace_sink {
                    agent = agent.with_trace(sink.clone());
                }
                agent.run(&c.symptom, |_| ()).await
            }
        };
        v.duration_sec = start.elapsed().as_secs_f64();
        let (final_text, turns, cache_rate, edit_count) = match res {
            Ok(r) => (
                r.final_text.clone(),
                r.turns,
                r.cache_hit_rate(),
                edit_tool.edits(),
            ),
            Err(e) => {
                v.error_kind = Some(match &e {
                    RunError::TurnBudget => CaseError::TurnBudgetExhausted,
                    RunError::Interrupted => CaseError::Interrupted,
                    RunError::Provider(_) => CaseError::ProviderFailure,
                });
                v.notes = format!("error: {e}");
                return v;
            }
        };
        v.turns = turns;
        v.cache_hit_rate = cache_rate;
        v.notes = truncate(&final_text, 200);

        // Score.
        let lower = final_text.to_lowercase();
        v.hallucination = c
            .hallucinated
            .iter()
            .any(|h| lower.contains(&h.to_lowercase()));

        if !c.gold.fix_applies.is_empty() && !c.gold.fix_must_contain.is_empty() {
            let path = workspace.path.join(&c.gold.fix_applies);
            if let Ok(got) = std::fs::read_to_string(&path) {
                let all_in = c.gold.fix_must_contain.iter().all(|s| got.contains(s));
                let none_bad = !c.gold.fix_must_not_have.iter().any(|s| got.contains(s));
                v.pass_ = all_in && none_bad;
            }
        }
        if c.gold.is_hardware_fault {
            v.partial = contains_diagnosis(&lower);
            // Pass requires BOTH a correct triage AND no edit_file calls.
            v.pass_ = v.partial && edit_count == 0;
        }
        if !c.gold.is_hardware_fault && c.gold.fix_applies.is_empty() {
            v.pass_ = false;
            v.notes = format!(
                "malformed gold: empty fix_applies for non-hardware-fault case {}",
                c.id
            );
        }
        v
    }
}

/// Aggregate metrics over a set of verdicts.
#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub total: usize,
    pub passed: usize,
    /// Harness errors (provider/workspace/interrupt) — distinct from model
    /// failures. The run book treats errored > 0 as not recordable (fix the
    /// harness, re-run); `decide()` itself is unchanged and still counts
    /// errored cases as failures.
    pub errored: usize,
    /// Turn-budget non-convergence — model behavior, not a harness error.
    pub non_converged: usize,
    pub partial: usize,
    pub hallucinated: usize,
    pub pass_rate: f64,
    pub hallucination_rate: f64,
    pub median_turns: u32,
    pub mean_cache_hit_rate: f64,
}

pub fn summarize(vs: &[Verdict]) -> Summary {
    let total = vs.len();
    if total == 0 {
        return Summary::default();
    }
    let passed = vs.iter().filter(|v| v.pass_).count();
    let partial = vs.iter().filter(|v| v.partial).count();
    let hallucinated = vs.iter().filter(|v| v.hallucination).count();
    let errored = vs
        .iter()
        .filter(|v| v.error_kind.map(|k| k.is_harness_error()).unwrap_or(false))
        .count();
    let non_converged = vs
        .iter()
        .filter(|v| v.error_kind == Some(CaseError::TurnBudgetExhausted))
        .count();
    let cache_sum: f64 = vs.iter().map(|v| v.cache_hit_rate).sum();
    let mut turns: Vec<u32> = vs.iter().map(|v| v.turns).collect();
    turns.sort_unstable();
    let median = turns[turns.len() / 2];
    Summary {
        total,
        passed,
        errored,
        non_converged,
        partial,
        hallucinated,
        pass_rate: passed as f64 / total as f64,
        hallucination_rate: hallucinated as f64 / total as f64,
        median_turns: median,
        mean_cache_hit_rate: cache_sum / total as f64,
    }
}

/// Apply the decision rule.
pub fn decide(s: &Summary, pass_thresh: f64, halluc_thresh: f64) -> &'static str {
    if s.pass_rate >= pass_thresh && s.hallucination_rate <= halluc_thresh {
        return "BUILD_LOCAL";
    }
    if s.pass_rate < 0.30 {
        return "PIVOT_TO_CLOUD";
    }
    "INCONCLUSIVE"
}

/// Word-phrase (not bare-substring) diagnosis matching — the red-team fix
/// against "default" matching "fault", "powered" matching "power".
fn contains_diagnosis(lower: &str) -> bool {
    const PHRASES: &[&str] = &[
        "hardware fault",
        "hardware issue",
        "wiring",
        "power supply",
        "psu",
        "brownout",
        "under-voltage",
        "undervoltage",
        "short circuit",
        "shorted",
        "stop coding",
        "physical fault",
        "not a software",
        "not a code",
        "rewire",
        "pull-up",
        "pull up resistor",
    ];
    PHRASES.iter().any(|p| lower.contains(p))
}

/// Unique per-case workspace. `remove_dir_all` on Drop after scoring and on
/// `Err` return. Honest bound: release `panic = "abort"` will not run Drop.
struct WorkspaceGuard {
    path: PathBuf,
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn sanitize_case_id(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "unknown".into()
    } else {
        s
    }
}

/// Create a unique temp dir seeded with `files`.
fn temp_workspace(
    case_id: &str,
    files: &std::collections::HashMap<String, String>,
) -> Result<WorkspaceGuard> {
    let dir = std::env::temp_dir().join(format!(
        "piforge-eval-{}-{}",
        std::process::id(),
        sanitize_case_id(case_id)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    // Guard before seeding: a mid-seed failure must still Drop-clean the dir
    // (it is pid-keyed — a later process will not reclaim it).
    let guard = WorkspaceGuard { path: dir.clone() };
    for (rel, content) in files {
        let abs = dir.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&abs, content)?;
    }
    Ok(guard)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.into()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push_str("...");
        t
    }
}

/// Scripted mock turns for all 13 case ids. Unknown ids get an empty terminal turn.
fn script(case_id: &str) -> Vec<MockTurn> {
    match case_id {
        "wrong-i2c-address-bme280-0x76" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 0),
            mturn_call("i2c", r#"{"action":"detect","address":118}"#, 200, 180),
            mturn_call("edit_file", r#"{"path":"bme_simpletest.py","content":"from board import *\nfrom adafruit_bme280 import basic as adafruit_bme280\ni2c = I2C(scl, sda)\nbme = adafruit_bme280.Adafruit_BME280_I2C(i2c, address=0x76)\nprint(bme.humidity)\n"}"#, 280, 260),
            mturn_text("Fixed: BME280 is at 0x76, not the default 0x77. Set address=0x76.", 320, 300),
        ],
        "i2c-wrong-address-0x77" => vec![
            mturn_call("hardware_inventory", r#"{}"#, 110, 0),
            mturn_call("i2c", r#"{"action":"scan"}"#, 180, 160),
            mturn_text("i2cdetect is empty even though the header has 3.3 V — SDA/SCL look swapped. This is a wiring fault. STOP coding; rewire the I2C pins.", 240, 220),
        ],
        "i2c-bus-scan-all-addresses" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 100),
            mturn_text("i2cdetect shows every address responding — that means SDA/SCL are shorted to power. This is a hardware/wiring fault. STOP coding; check the wiring and pull-ups before any further I2C op.", 200, 180),
        ],
        "undervoltage-brownout" => vec![
            mturn_call("telemetry", r#"{"action":"snapshot"}"#, 130, 110),
            mturn_text("vcgencmd get_throttled shows undervoltage has occurred (bit 16). This is a power-supply problem, not code — use a 5V/3A+ PSU and don't power servos from the 3.3V rail. STOP coding.", 210, 190),
        ],
        "gpio-board-vs-bcm-numbering" => vec![
            mturn_call("edit_file", r#"{"path":"blink.py","content":"from gpiozero import LED\nfrom time import sleep\nled = LED(17)\nwhile True:\n    led.on()\n    sleep(0.5)\n    led.off()\n    sleep(0.5)\n"}"#, 200, 180),
            mturn_text("gpiozero uses BCM numbering. Physical pin 11 is BCM 17; use LED(17).", 260, 240),
        ],
        "pi5-rpigpio-migration-break" => vec![
            mturn_call("edit_file", r#"{"path":"app.py","content":"from gpiozero import LED\nled = LED(17)\nled.on()\n"}"#, 200, 180),
            mturn_text("RPi.GPIO cannot drive Pi 5 GPIO. Rewrote with gpiozero.", 260, 240),
        ],
        "servo-jitter-software-pwm" => vec![
            mturn_call("edit_file", r#"{"path":"servo.py","content":"from gpiozero import Servo\nfrom time import sleep\nservo = Servo(17)\nwhile True:\n    sleep(1)\n"}"#, 200, 180),
            mturn_text("Software PWM jitters under Linux. Use gpiozero Servo (hardware-timed on Pi 5).", 260, 240),
        ],
        "bmp280-vs-bme280-chipid" => vec![
            mturn_call("i2c", r#"{"action":"read","address":118,"register":208,"length":1}"#, 120, 0),
            mturn_call("edit_file", r#"{"path":"bme.py","content":"from board import *\nimport adafruit_bmp280\ni2c = I2C(scl, sda)\nbmp = adafruit_bmp280.Adafruit_BMP280_I2C(i2c, address=0x76)\nprint(bmp.pressure)\n"}"#, 200, 180),
            mturn_text("Chip-id 0x58 at 0xD0 is a BMP280, not a BME280. Switched to the BMP280 driver.", 260, 240),
        ],
        "bme280-pressure-unit-conversion" => vec![
            mturn_call("edit_file", r#"{"path":"bme_read.py","content":"def read_pressure(reg_bytes):\n    raw = int.from_bytes(reg_bytes, 'big')\n    return raw * 100  # hPa to Pa\n"}"#, 200, 180),
            mturn_text("Pressure was in hPa; multiply by 100 to report Pa.", 260, 240),
        ],
        "i2c-not-enabled" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 0),
            mturn_call("edit_file", r#"{"path":"config.txt","content":"arm_64bit=1\ndtparam=i2c_arm=on\n"}"#, 200, 180),
            mturn_text("I2C overlay was off. Added dtparam=i2c_arm=on to workspace config.txt.", 260, 240),
        ],
        "ds18b20-1wire-overlay-missing" => vec![
            mturn_call("hardware_inventory", r#"{}"#, 110, 0),
            mturn_call("edit_file", r#"{"path":"config.txt","content":"arm_64bit=1\ndtoverlay=w1-gpio\n"}"#, 200, 180),
            mturn_text("1-wire overlay missing. Added dtoverlay=w1-gpio to workspace config.txt.", 260, 240),
        ],
        "iio-scale-misapply" => vec![
            mturn_call("hardware_inventory", r#"{}"#, 110, 0),
            mturn_call("edit_file", r#"{"path":"adc_read.py","content":"raw = int(open(\"/sys/bus/iio/devices/iio:device0/in_voltage0_raw\").read())\nscale = float(open(\"/sys/bus/iio/devices/iio:device0/in_voltage_scale\").read())\nprint(raw * scale / 1000)\n"}"#, 200, 180),
            mturn_text("Raw IIO counts must be multiplied by in_voltage_scale.", 260, 240),
        ],
        "i2c-device-not-found-timeout" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 0),
            mturn_call("edit_file", r#"{"path":"mpu.py","content":"from mpu6050 import MPU6050\nsensor = MPU6050(address=0x68)\nprint(sensor.get_accel_data())\n"}"#, 200, 180),
            mturn_text("MPU6050 is at address=0x68, not 0x69.", 260, 240),
        ],
        _ => vec![mturn_text("", 100, 0)],
    }
}

fn mturn_call(name: &str, args: &str, prompt: u64, cached: u64) -> MockTurn {
    MockTurn {
        tool_calls: vec![ToolCall {
            id: format!("{name}-1"),
            kind: "function".into(),
            function: crate::provider::ToolCallFunction {
                name: name.into(),
                arguments: args.into(),
            },
        }],
        text: String::new(),
        prompt_tokens: prompt,
        completion: 50,
        cached,
    }
}
fn mturn_text(text: &str, prompt: u64, cached: u64) -> MockTurn {
    MockTurn {
        tool_calls: vec![],
        text: text.into(),
        prompt_tokens: prompt,
        completion: 50,
        cached,
    }
}

#[cfg(test)]
mod script_invariants {
    use super::*;

    fn cases_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../eval/cases")
    }

    fn load_cases() -> Vec<Case> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(cases_dir()).expect("eval/cases") {
            let path = e.expect("entry").path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let raw = std::fs::read_to_string(&path).unwrap();
            out.push(
                serde_json::from_str(&raw)
                    .unwrap_or_else(|err| panic!("parse {}: {err}", path.display())),
            );
        }
        out
    }

    fn script_blob(turns: &[MockTurn]) -> String {
        let mut s = String::new();
        for t in turns {
            s.push_str(&t.text);
            s.push('\n');
            for call in &t.tool_calls {
                s.push_str(&call.function.name);
                s.push(' ');
                s.push_str(&call.function.arguments);
                s.push('\n');
            }
        }
        s
    }

    #[test]
    fn scripts_cover_all_ids_and_avoid_hallucinated() {
        let cases = load_cases();
        assert_eq!(cases.len(), 13);
        for c in &cases {
            let turns = script(&c.id);
            assert!(
                !turns.is_empty()
                    && turns
                        .iter()
                        .any(|t| !t.tool_calls.is_empty() || !t.text.is_empty()),
                "{}: expected a real script, not the empty fallback",
                c.id
            );
            let text_blob = turns
                .iter()
                .map(|t| t.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
            for h in &c.hallucinated {
                assert!(
                    !text_blob.contains(&h.to_lowercase()),
                    "{} terminal text must not contain hallucinated {h:?}\n{}",
                    c.id,
                    turns
                        .iter()
                        .map(|t| t.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
            let used_edit = turns.iter().any(|t| {
                t.tool_calls
                    .iter()
                    .any(|call| call.function.name == "edit_file")
            });
            if c.gold.is_hardware_fault {
                assert!(!used_edit, "{} STOP script must not call edit_file", c.id);
            } else {
                assert!(used_edit, "{} file-fix script must call edit_file", c.id);
                let edits: String = turns
                    .iter()
                    .flat_map(|t| t.tool_calls.iter())
                    .filter(|call| call.function.name == "edit_file")
                    .map(|call| call.function.arguments.clone())
                    .collect();
                for needle in &c.gold.fix_must_contain {
                    assert!(
                        edits.contains(needle),
                        "{} edit_file must contain gold {needle:?}\n{edits}",
                        c.id
                    );
                }
            }
        }
        let chip = script("bmp280-vs-bme280-chipid");
        let chip_blob = script_blob(&chip);
        assert!(
            chip_blob.contains("\"register\":208") || chip_blob.contains("0xD0"),
            "chip-id script must read register 0xD0, got {chip_blob}"
        );
    }
}
