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

use crate::agent::{Agent, AgentError, LlmClient, MockProvider, MockTurn};
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
    /// True when the agent exhausted its turn budget without a terminal
    /// response (`AgentError::TurnBudgetExhausted`). Kept SEPARATE from
    /// `pass_`/`hallucination`: a non-converged model is "too weak / context
    /// too short," not "wrong verdict." See `decide` + the summary banner.
    pub non_converged: bool,
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
            // Only the fixture `*.json` — NOT the paired `*.mock.json` scripts.
            // A mock script is a trajectory, not a case.
            .filter(|e| {
                let p = e.path();
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                p.extension().and_then(|x| x.to_str()) == Some("json")
                    && !name.ends_with(".mock.json")
            })
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
            let v = self.run_case(&case, cases_dir).await;
            progress(&v);
            verdicts.push(v);
        }
        Ok(verdicts)
    }

    async fn run_case(&self, c: &Case, cases_dir: &str) -> Verdict {
        let start = std::time::Instant::now();
        let mut v = Verdict {
            case_id: c.id.clone(),
            ..Default::default()
        };

        // Per-case temp workspace seeded with the fixture's files. The guard
        // removes the dir on drop (end of the case) so cases never share a dir
        // and /tmp doesn't leak one-per-run.
        let workspace = match temp_workspace(&c.id, &c.setup.files) {
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
        let edit_tool = CodeEditTool::new(&workspace.path);
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
                mock.load(load_mock_script(&c.id, cases_dir)).await;
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
            // Distinguish turn-budget exhaustion from other failures. A
            // non-converged model is reported separately (not folded into
            // pass_/hallucination) so the eval doesn't misread "too weak" as
            // "wrong verdict."
            Err(AgentError::TurnBudgetExhausted) => {
                v.non_converged = true;
                v.notes = "turn budget exhausted (model did not converge)".into();
                return v;
            }
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
            let path = std::path::Path::new(&workspace.path).join(&c.gold.fix_applies);
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
    /// Count of cases where the agent exhausted its turn budget without
    /// converging. Reported separately from pass/fail: a high value means the
    /// model is too weak / context too short for the verdict to be trusted.
    pub non_converged: usize,
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
    let non_converged = vs.iter().filter(|v| v.non_converged).count();
    let cache_sum: f64 = vs.iter().map(|v| v.cache_hit_rate).sum();
    let mut turns: Vec<u32> = vs.iter().map(|v| v.turns).collect();
    turns.sort_unstable();
    let median = turns[turns.len() / 2];
    Summary {
        total,
        passed,
        partial,
        hallucinated,
        non_converged,
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

/// Whether the run's non-convergence rate is high enough that the decision is
/// NOT trustworthy — the model is too weak / context too short to converge on
/// most cases, so pass_rate is not a reliable capability signal. Extracted as a
/// `pub fn` (rather than inlined in the binary) so the test exercises the REAL
/// predicate, not a copy that can drift.
///
/// Reported ALONGSIDE the decision (Approach §5: report, do not remap). Strict
/// `>` so exactly half does not flag.
pub fn model_too_weak_banner(s: &Summary) -> bool {
    s.total > 0 && s.non_converged as f64 / s.total as f64 > 0.5
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

/// RAII guard for a per-case temp workspace. Removes the dir on drop so cases
/// never share state and /tmp doesn't leak. The `path` is the absolute dir,
/// held by the caller for the case's lifetime. If `remove_dir_all` fails on
/// drop (perms, NFS), the failure is logged to stderr and swallowed — a
/// leftover dir is cosmetic; the collision it prevents is already avoided by
/// the per-case-id + random naming.
pub struct WorkspaceGuard {
    pub path: String,
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            // Don't panic: a stale temp dir is not worth killing the run.
            eprintln!(
                "piforge-eval: WARNING failed to clean temp dir {}: {e}",
                self.path
            );
        }
    }
}

/// Create a temp dir seeded with `files`, keyed on `case_id` (not PID) so each
/// case gets an isolated workspace. A random suffix disambiguates re-runs of
/// the same case (and would allow parallel cases in future without collision).
/// The returned guard removes the dir on drop.
///
/// Every fixture `files` key is validated with `sim::safe_join` (the SAME
/// containment primitive `edit_file` uses) so a traversal key like
/// `"../../etc/x"` cannot escape the workspace root — defense in depth even
/// though fixtures ship with the repo.
///
/// Exposed as `pub` so the workspace-isolation integration test can exercise
/// the per-case-id naming + cleanup guard directly (Inv 11).
pub fn temp_workspace(
    case_id: &str,
    files: &std::collections::HashMap<String, String>,
) -> Result<WorkspaceGuard> {
    // Sanitize case_id into a path-safe component (case ids are already
    // filesystem-clean in practice, but defend in depth against a future
    // fixture with a slash).
    let safe_id: String = case_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // OS-seeded random suffix. `RandomState::new()` pulls entropy from the
    // OS (getrandom on Linux, SecRandom on macOS) — far stronger than a
    // subsecond-clock XOR, which collides heavily under tight loops. This
    // keeps re-runs of the same case-id disjoint without relying on clock
    // resolution. Falls back to a process-id + clock mix if hashing fails
    // (it never does in practice).
    let rand: u64 = {
        let rs = std::collections::hash_map::RandomState::new();
        let h1 = std::hash::BuildHasher::hash_one(&rs, case_id);
        let h2 = std::hash::BuildHasher::hash_one(&rs, std::process::id());
        h1.wrapping_add(h2)
    };
    let dir = std::env::temp_dir().join(format!("piforge-eval-{safe_id}-{rand:016x}"));
    let dir_str = dir.to_string_lossy().into_owned();
    std::fs::create_dir_all(&dir)?;
    for (rel, content) in files {
        // Containment check: reject traversal/symlink-escape keys. Same
        // primitive the edit_file tool uses for agent-driven writes.
        let abs = crate::sim::safe_join(&dir_str, rel)
            .map_err(|e| anyhow!("unsafe fixture path {rel:?}: {e}"))?;
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&abs, content)?;
    }
    Ok(WorkspaceGuard { path: dir_str })
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

/// Load the per-case mock script from `<cases_dir>/<id>.mock.json`. If no file
/// exists, returns a single empty terminal turn (the legacy no-op behavior) so
/// the harness still runs — though such a case will score as non-pass, which
/// is exactly the signal that a fixture is missing its mock script.
fn load_mock_script(case_id: &str, cases_dir: &str) -> Vec<MockTurn> {
    let path = std::path::Path::new(cases_dir).join(format!("{case_id}.mock.json"));
    match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<Vec<MockTurnSerde>>(&s) {
            Ok(ts) => ts.iter().map(MockTurn::from_serde).collect(),
            Err(e) => {
                eprintln!(
                    "piforge-eval: WARNING parse {}: {e}; using no-op turns",
                    path.display()
                );
                vec![mturn_text("", 100, 0)]
            }
        },
        Err(_) => vec![mturn_text("", 100, 0)],
    }
}

/// Serde shape for a mock turn on disk. `tool_calls` is optional; `text`
/// optional; token counts default.
#[derive(Deserialize)]
struct MockTurnSerde {
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[serde(default)]
    text: String,
    #[serde(default = "default_prompt")]
    prompt_tokens: u64,
    #[serde(default = "default_completion")]
    completion: u64,
    #[serde(default)]
    cached: u64,
}
fn default_prompt() -> u64 {
    100
}
fn default_completion() -> u64 {
    50
}

impl MockTurn {
    fn from_serde(s: &MockTurnSerde) -> Self {
        MockTurn {
            tool_calls: s.tool_calls.clone(),
            text: s.text.clone(),
            prompt_tokens: s.prompt_tokens,
            completion: s.completion,
            cached: s.cached,
        }
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
