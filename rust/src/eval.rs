//! The capability eval — the gate that must pass before the local-appliance
//! product is built. Cases run headless: the agent's tool calls are served by
//! the sim package from each Case fixture, edits land in a per-case temp dir,
//! and a mock provider lets it all run in CI without a llama-server.
//!
//! Decision rule: ≥55% fix-rate AND <10% register/pin hallucination → BUILD_LOCAL.
use std::path::Path;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::agent::{Agent, LlmClient, MockProvider, MockTurn};
use crate::broker::{Broker, ThrottledReader};
use crate::config::{Config, EvalConfig};
use crate::hil::{Tool, ToolVec};
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

/// Per-case verdict.
#[derive(Debug, Clone, Default)]
pub struct Verdict {
    pub case_id: String,
    pub pass_: bool,
    pub partial: bool,
    pub hallucination: bool,
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
}

impl Runner {
    pub fn new(client: std::sync::Arc<dyn LlmClient>, max_turns: u32) -> Self {
        Self {
            client: Some(client),
            mock: None,
            max_turns,
        }
    }
    pub fn new_mock(max_turns: u32) -> (Self, std::sync::Arc<MockProvider>) {
        let mock = std::sync::Arc::new(MockProvider::new());
        (
            Self {
                client: None,
                mock: Some(mock.clone()),
                max_turns,
            },
            mock,
        )
    }

    /// Run all *.json cases in `cases_dir`, invoking `progress` per verdict.
    pub async fn run_all<F>(&self, cases_dir: &str, mut progress: F) -> Result<Vec<Verdict>>
    where
        F: FnMut(&Verdict),
    {
        let mut entries: Vec<_> = std::fs::read_dir(cases_dir)
            .map_err(|e| anyhow!("read cases dir {cases_dir}: {e}"))?
            .filter_map(|e| e.ok())
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
        let workspace = match temp_workspace(&c.setup.files) {
            Ok(w) => w,
            Err(e) => {
                v.notes = format!("error: {e}");
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
                per_pin_max_current_ma: 12,
                rail_budget_ma: 50,
            },
            Some(st_reader),
            Broker::always_deny(),
        ));
        let edit_tool = CodeEditTool::new(&workspace);
        let tools: ToolVec = vec![
            sim::InventoryTool::new(st.clone()),
            sim::TelemetryTool::new(st.clone()),
            sim::I2CTool::new(st.clone()),
            sim::GPIOTool::new(st.clone(), Some(gate)),
            sim::ScopeTool::new(st.clone()),
            edit_tool.clone(),
        ];

        let res = match &self.client {
            Some(client) => {
                let agent = Agent::new(client.clone(), tools, self.max_turns);
                agent.run(&c.symptom, |_| ()).await
            }
            None => {
                let mock = self.mock.clone().unwrap();
                mock.load(script(&c.id)).await;
                let agent =
                    Agent::new(mock as std::sync::Arc<dyn LlmClient>, tools, self.max_turns);
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
            let path = std::path::Path::new(&workspace).join(&c.gold.fix_applies);
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
        v
    }
}

/// Aggregate metrics over a set of verdicts.
#[derive(Debug, Default, Clone)]
pub struct Summary {
    pub total: usize,
    pub passed: usize,
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
    let cache_sum: f64 = vs.iter().map(|v| v.cache_hit_rate).sum();
    let mut turns: Vec<u32> = vs.iter().map(|v| v.turns).collect();
    turns.sort_unstable();
    let median = turns[turns.len() / 2];
    Summary {
        total,
        passed,
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

/// Create a temp dir seeded with `files`; returns its path. Caller cleans up.
fn temp_workspace(files: &std::collections::HashMap<String, String>) -> Result<String> {
    let dir = std::env::temp_dir().join(format!("piforge-eval-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    for (rel, content) in files {
        let abs = dir.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&abs, content)?;
    }
    Ok(dir.to_string_lossy().into_owned())
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

/// Scripted mock turns for the seed cases. Returns an
/// empty terminal turn for any case without a script.
fn script(case_id: &str) -> Vec<MockTurn> {
    match case_id {
        "wrong-i2c-address-bme280-0x76" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 0),
            mturn_call("i2c", r#"{"action":"detect","address":118}"#, 200, 180),
            mturn_call("edit_file", &format!(r#"{{"path":"bme_simpletest.py","content":"from board import *\nfrom adafruit_bme280 import basic as adafruit_bme280\ni2c = I2C(scl, sda)\nbme = adafruit_bme280.Adafruit_BME280_I2C(i2c, address=0x76)\nprint(bme.humidity)\n"}}"#), 280, 260),
            mturn_text("Fixed: BME280 is at 0x76, not the default 0x77. Set address=0x76.", 320, 300),
        ],
        "i2c-bus-scan-all-addresses" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 100),
            mturn_text("i2cdetect shows every address responding — that means SDA/SCL are shorted to power. This is a hardware/wiring fault. STOP coding; check the wiring and pull-ups before any further I2C op.", 200, 180),
        ],
        "undervoltage-brownout" => vec![
            mturn_call("telemetry", r#"{"action":"snapshot"}"#, 130, 110),
            mturn_text("vcgencmd get_throttled shows undervoltage has occurred (bit 16). This is a power-supply problem, not code — use a 5V/3A+ PSU and don't power servos from the 3.3V rail. STOP coding.", 210, 190),
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

// keep the unused-config import warning quiet when only mock mode is exercised
#[allow(dead_code)]
fn _unused(_c: &Config, _e: &EvalConfig) {}
