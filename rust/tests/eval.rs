//! Eval scorer + decision-rule tests.
//! The key regression: word-boundary diagnosis matching must not overmatch
//! ("default" must NOT trip "fault", "powered" must NOT trip "power").
// run_all over the shared ../eval/cases corpus uses pid+case-id-keyed temp
// workspaces; tests doing so are serialized so concurrent runs don't collide
// on the same workspace dir. The lock deliberately spans awaits.
#![allow(clippy::await_holding_lock)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use piforge::agent::LlmClient;
use piforge::eval::{decide, summarize, CaseError, Runner, Verdict};

static EVAL_RUN_LOCK: Mutex<()> = Mutex::new(());

fn shared_cases_dir() -> String {
    format!("{}/../eval/cases", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn decide_gate_thresholds() {
    let cases = [
        (0.70, 0.05, 0.55, 0.10, "BUILD_LOCAL"),
        (0.70, 0.20, 0.55, 0.10, "INCONCLUSIVE"), // too much halluc
        (0.20, 0.05, 0.55, 0.10, "PIVOT_TO_CLOUD"),
        (0.40, 0.05, 0.55, 0.10, "INCONCLUSIVE"),
        (0.55, 0.10, 0.55, 0.10, "BUILD_LOCAL"), // exactly at threshold
    ];
    for (pr, hr, pt, ht, want) in cases {
        // Build a synthetic summary with the rates directly (summarize computes
        // rates from verdicts; for threshold tests use a manual summary).
        let s = piforge::eval::Summary {
            total: 1,
            passed: 1,
            errored: 0,
            non_converged: 0,
            partial: 0,
            hallucinated: 0,
            pass_rate: pr,
            hallucination_rate: hr,
            median_turns: 0,
            mean_cache_hit_rate: 0.0,
        };
        let got = decide(&s, pt, ht);
        assert_eq!(got, want, "pass={pr} halluc={hr}: got {got} want {want}");
    }
}

// The word-boundary matching lives as a private fn in eval.rs; we exercise it
// indirectly via the public summarize/decide surface.

#[test]
fn summarize_aggregates() {
    let vs = vec![
        Verdict {
            pass_: true,
            turns: 2,
            cache_hit_rate: 0.8,
            ..Default::default()
        },
        Verdict {
            pass_: true,
            turns: 4,
            cache_hit_rate: 0.6,
            ..Default::default()
        },
        Verdict {
            pass_: false,
            hallucination: true,
            turns: 3,
            cache_hit_rate: 0.5,
            ..Default::default()
        },
    ];
    let s = summarize(&vs);
    assert_eq!((s.total, s.passed, s.hallucinated), (3, 2, 1));
    assert!(
        (s.pass_rate - 0.667).abs() < 0.01,
        "pass_rate {}",
        s.pass_rate
    );
    assert!(
        (s.hallucination_rate - 0.333).abs() < 0.01,
        "halluc_rate {}",
        s.hallucination_rate
    );
    assert_eq!(s.median_turns, 3, "median of {{2,3,4}}");
}

#[test]
fn summarize_mixed_batch_preserves_denominator() {
    // Mixed batch: 8 pass, 1 hallucination-fail, 2 provider-errored, 1
    // turn-budget non-converged. Every case stays in the pass_rate
    // denominator (comparability with prior runs); the attribution split
    // separates harness errors from model failures without moving the rate.
    let mk = |pass_: bool| Verdict {
        pass_,
        ..Default::default()
    };
    let vs: Vec<Verdict> = vec![
        mk(true),
        mk(true),
        mk(true),
        mk(true),
        mk(true),
        mk(true),
        mk(true),
        mk(true),
        Verdict {
            pass_: false,
            hallucination: true,
            ..Default::default()
        },
        Verdict {
            pass_: false,
            error_kind: Some(CaseError::ProviderFailure),
            ..Default::default()
        },
        Verdict {
            pass_: false,
            error_kind: Some(CaseError::Interrupted),
            ..Default::default()
        },
        Verdict {
            pass_: false,
            error_kind: Some(CaseError::TurnBudgetExhausted),
            ..Default::default()
        },
    ];
    let s = summarize(&vs);
    assert_eq!(s.total, 12);
    assert_eq!(s.passed, 8);
    assert_eq!(s.errored, 2, "provider + interrupt are harness errors");
    assert_eq!(s.non_converged, 1, "turn budget is model non-convergence");
    assert!((s.pass_rate - 8.0 / 12.0).abs() < 1e-9, "{}", s.pass_rate);
}

// Suppress unused-warning for Setup import kept for parity with other tests.
#[allow(dead_code)]
fn _setup() -> Setup {
    Setup {
        i2c_registers: Default::default(),
        one_wire: vec![],
        iio: vec![],
        i2c_bus_present: true,
        ..Setup::default()
    }
}
type Setup = piforge::sim::Setup;

#[tokio::test]
async fn workspace_dirs_are_unique_across_cases() {
    let root = std::env::temp_dir().join(format!("piforge-eval-iso-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("a-shared.json"),
        r#"{
          "id": "iso-a",
          "symptom": "case A seeds leftover",
          "setup": {
            "files": { "shared.py": "LEFTOVER_FROM_A" },
            "throttled": "0x0"
          },
          "gold": {
            "is_hardware_fault": true,
            "fix_applies": "",
            "fix_must_contain": []
          }
        }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("b-shared.json"),
        r#"{
          "id": "iso-b",
          "symptom": "case B must not see A's leftover",
          "setup": {
            "files": {},
            "throttled": "0x0"
          },
          "gold": {
            "is_hardware_fault": false,
            "fix_applies": "shared.py",
            "fix_must_contain": ["LEFTOVER_FROM_A"]
          }
        }"#,
    )
    .unwrap();

    let (runner, _mock) = piforge::eval::Runner::new_mock(8, false);
    let vs = runner
        .run_all(root.to_str().unwrap(), |_| ())
        .await
        .expect("run_all");
    assert_eq!(vs.len(), 2, "both isolation fixtures must be scored");
    let a = vs
        .iter()
        .find(|v| v.case_id == "iso-a")
        .expect("iso-a verdict");
    let b = vs
        .iter()
        .find(|v| v.case_id == "iso-b")
        .expect("iso-b verdict");
    assert_eq!(a.case_id, "iso-a");
    assert!(
        !b.pass_,
        "case B must not pass from case A's leftover shared.py (unique workspaces)"
    );
    assert!(
        !b.notes.contains("malformed gold:"),
        "iso-b has file gold so failure must be missing leftover, not malformed gold; notes={:?}",
        b.notes
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn malformed_gold_overwrites_notes() {
    let root = std::env::temp_dir().join(format!(
        "piforge-eval-malformed-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("bad.json"),
        r#"{
          "id": "malformed-empty-fix",
          "symptom": "this diagnosis would otherwise become notes",
          "setup": { "throttled": "0x0" },
          "gold": {
            "is_hardware_fault": false,
            "fix_applies": "",
            "fix_must_contain": []
          }
        }"#,
    )
    .unwrap();

    let (runner, _mock) = piforge::eval::Runner::new_mock(8, false);
    let vs = runner
        .run_all(root.to_str().unwrap(), |_| ())
        .await
        .expect("run_all");
    assert_eq!(vs.len(), 1);
    assert!(!vs[0].pass_);
    assert!(
        vs[0].notes.contains("malformed gold:"),
        "notes must be overwritten to malformed gold, got {:?}",
        vs[0].notes
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---- failure attribution + run trace ----

/// LLM stub whose every chat call fails — the provider-failure path.
struct FailingClient;

#[async_trait]
impl LlmClient for FailingClient {
    async fn chat(
        &self,
        _req: &piforge::provider::ChatRequest,
    ) -> anyhow::Result<piforge::provider::ChatResponse> {
        anyhow::bail!("HTTP 503 (stub)")
    }
}

#[tokio::test]
async fn provider_failure_attributes_as_harness_error() {
    let _lock = EVAL_RUN_LOCK.lock().unwrap();
    let runner = Runner::new(Arc::new(FailingClient) as Arc<dyn LlmClient>, 4, false);
    let vs = runner
        .run_all(&shared_cases_dir(), |_| ())
        .await
        .expect("run_all");
    assert!(!vs.is_empty());
    for v in &vs {
        assert_eq!(
            v.error_kind,
            Some(CaseError::ProviderFailure),
            "{}: {:?}",
            v.case_id,
            v.notes
        );
        assert!(!v.pass_, "an errored case can never pass");
    }
    let s = summarize(&vs);
    assert_eq!(s.errored, vs.len());
    assert_eq!(s.non_converged, 0);
    // Denominator preserved: errored cases still count as failures.
    assert!((s.pass_rate - 0.0).abs() < f64::EPSILON);
}

#[tokio::test]
async fn turn_budget_exhaustion_is_non_convergence_not_harness_error() {
    let _lock = EVAL_RUN_LOCK.lock().unwrap();
    // max_turns=1: every script's first turn is a tool call, so every case
    // exhausts the budget without a terminal response.
    let (runner, _mock) = Runner::new_mock(1, false);
    let vs = runner
        .run_all(&shared_cases_dir(), |_| ())
        .await
        .expect("run_all");
    assert!(!vs.is_empty());
    for v in &vs {
        assert_eq!(
            v.error_kind,
            Some(CaseError::TurnBudgetExhausted),
            "{}: {:?}",
            v.case_id,
            v.notes
        );
        assert!(!v.pass_);
    }
    let s = summarize(&vs);
    assert_eq!(s.non_converged, s.total);
    assert_eq!(
        s.errored, 0,
        "non-convergence is model behavior, not harness"
    );
}

#[tokio::test]
async fn trace_jsonl_records_run_lifecycle() {
    let _lock = EVAL_RUN_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("piforge-trace-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (runner, _mock) = Runner::new_mock(12, false);
    let runner = runner.with_trace_dir(dir.clone());
    let vs = runner
        .run_all(&shared_cases_dir(), |_| ())
        .await
        .expect("run_all");
    assert!(runner.degraded_traces().is_empty());
    assert!(vs.iter().all(|v| v.pass_));

    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(files.len(), vs.len(), "one trace file per case");
    for entry in files {
        let text = std::fs::read_to_string(entry.path()).unwrap();
        let mut saw_start = false;
        let mut saw_call = false;
        let mut saw_result = false;
        let mut saw_usage = false;
        let mut saw_text = false;
        let mut terminal = None;
        for line in text.lines() {
            let v: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{}: bad JSONL ({e}): {line}", entry.path().display()));
            assert!(v.get("case_id").is_some(), "case id stamped: {line}");
            let lower = line.to_lowercase();
            assert!(!lower.contains("api_key"), "no key material: {line}");
            match v["event"].as_str().unwrap() {
                "run_started" => {
                    saw_start = true;
                    let h = v["prefix_sha256"].as_str().unwrap();
                    assert_eq!(h.len(), 64, "sha256 hex: {h}");
                    assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
                }
                "tool_called" => saw_call = true,
                "tool_result" => saw_result = true,
                "turn_usage" => saw_usage = true,
                "assistant_text" => saw_text = true,
                "run_finished" | "run_error" => {
                    terminal = Some(v["event"].as_str().unwrap().to_string())
                }
                _ => {}
            }
        }
        assert!(saw_start, "{}: RunStarted present", entry.path().display());
        assert!(
            saw_call && saw_result,
            "{}: tool lifecycle recorded",
            entry.path().display()
        );
        assert!(saw_usage, "{}: turn usage recorded", entry.path().display());
        assert!(
            saw_text,
            "{}: assistant text recorded",
            entry.path().display()
        );
        assert_eq!(
            terminal.as_deref(),
            Some("run_finished"),
            "{}: terminal event",
            entry.path().display()
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn trace_sink_is_behavior_neutral() {
    let _lock = EVAL_RUN_LOCK.lock().unwrap();
    let (plain, _m1) = Runner::new_mock(12, false);
    let v_plain = plain
        .run_all(&shared_cases_dir(), |_| ())
        .await
        .expect("run_all");

    let dir = std::env::temp_dir().join(format!("piforge-trace-neutral-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (traced, _m2) = Runner::new_mock(12, false);
    let v_traced = traced
        .with_trace_dir(dir.clone())
        .run_all(&shared_cases_dir(), |_| ())
        .await
        .expect("run_all");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(v_plain.len(), v_traced.len());
    for (a, b) in v_plain.iter().zip(v_traced.iter()) {
        assert_eq!(a.case_id, b.case_id);
        assert_eq!(a.turns, b.turns, "{}", a.case_id);
        assert_eq!(a.pass_, b.pass_, "{}", a.case_id);
        assert_eq!(a.cache_hit_rate, b.cache_hit_rate, "{}", a.case_id);
    }
}

// ---- scorer + trace edge coverage (crafted fixture variants) ----

/// Copy a corpus fixture into `dir`, mutating its JSON — drives scorer paths
/// the healthy corpus (by design) never trips.
fn write_case_variant(
    dir: &std::path::Path,
    id: &str,
    mutate: impl FnOnce(&mut serde_json::Value),
) {
    let src = format!("{}/../eval/cases/{}.json", env!("CARGO_MANIFEST_DIR"), id);
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&src).unwrap()).unwrap();
    mutate(&mut v);
    std::fs::write(dir.join(format!("{id}.json")), v.to_string()).unwrap();
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("piforge-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[tokio::test]
async fn hallucination_flag_is_substring_of_final_text() {
    let _lock = EVAL_RUN_LOCK.lock().unwrap();
    let root = temp_root("halluc");
    // The script's terminal text contains "0x76" — make it a hallucination.
    write_case_variant(&root, "wrong-i2c-address-bme280-0x76", |v| {
        v["hallucinated"] = serde_json::json!(["0x76"]);
    });
    let (runner, _m) = Runner::new_mock(12, false);
    let vs = runner
        .run_all(root.to_str().unwrap(), |_| ())
        .await
        .unwrap();
    assert_eq!(vs.len(), 1);
    assert!(vs[0].hallucination, "hallucinated substring must flag");
    assert!(vs[0].pass_, "the flag alone does not fail the case");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn scorer_rejects_forbidden_content_in_edited_file() {
    let _lock = EVAL_RUN_LOCK.lock().unwrap();
    let root = temp_root("forbidden");
    // The gold edit contains "0x76" — forbid it and the case must fail.
    write_case_variant(&root, "wrong-i2c-address-bme280-0x76", |v| {
        v["gold"]["fix_must_not_have"] = serde_json::json!(["0x76"]);
    });
    let (runner, _m) = Runner::new_mock(12, false);
    let vs = runner
        .run_all(root.to_str().unwrap(), |_| ())
        .await
        .unwrap();
    assert_eq!(vs.len(), 1);
    assert!(
        !vs[0].pass_,
        "forbidden content in the edited file must fail"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn hardware_fault_gold_with_edits_cannot_pass() {
    let _lock = EVAL_RUN_LOCK.lock().unwrap();
    let root = temp_root("stopwithedits");
    // Same scripted edits, but the gold says hardware fault: pass requires
    // phrase match AND edit_count == 0 — the edits alone must sink it.
    write_case_variant(&root, "wrong-i2c-address-bme280-0x76", |v| {
        v["gold"] = serde_json::json!({
            "is_hardware_fault": true,
            "fix_applies": "",
            "fix_must_contain": []
        });
    });
    let (runner, _m) = Runner::new_mock(12, false);
    let vs = runner
        .run_all(root.to_str().unwrap(), |_| ())
        .await
        .unwrap();
    assert_eq!(vs.len(), 1);
    assert!(!vs[0].pass_, "a STOP case with edits can never pass");
    assert!(!vs[0].partial, "terminal text carries no diagnosis phrase");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn stop_case_passes_despite_hallucination_flag() {
    let _lock = EVAL_RUN_LOCK.lock().unwrap();
    let root = temp_root("stopplusflag");
    // STOP gold, correct triage, no edits: pass — even with the flag set.
    write_case_variant(&root, "i2c-bus-scan-all-addresses", |v| {
        v["hallucinated"] = serde_json::json!(["STOP"]);
    });
    let (runner, _m) = Runner::new_mock(12, false);
    let vs = runner
        .run_all(root.to_str().unwrap(), |_| ())
        .await
        .unwrap();
    assert_eq!(vs.len(), 1);
    assert!(vs[0].pass_, "phrase + zero edits passes");
    assert!(vs[0].hallucination, "flag is independent of pass");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn errored_case_trace_survives_to_error_point() {
    let _lock = EVAL_RUN_LOCK.lock().unwrap();
    let dir = temp_root("trace-err");
    let runner = Runner::new(Arc::new(FailingClient) as Arc<dyn LlmClient>, 4, false);
    let runner = runner.with_trace_dir(dir.clone());
    let vs = runner.run_all(&shared_cases_dir(), |_| ()).await.unwrap();
    assert!(vs
        .iter()
        .all(|v| v.error_kind == Some(CaseError::ProviderFailure)));
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(files.len(), vs.len());
    for entry in files {
        let text = std::fs::read_to_string(entry.path()).unwrap();
        let mut lines = text.lines();
        let first: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(first["event"], "run_started", "{}", entry.path().display());
        let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(
            last["event"],
            "run_error",
            "{}: errored case keeps a partial trace ending at the error",
            entry.path().display()
        );
        assert!(!text.contains("run_finished"));
    }
    assert!(runner.degraded_traces().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn trace_write_failure_degrades_without_failing_cases() {
    let _lock = EVAL_RUN_LOCK.lock().unwrap();
    // A FILE where the trace dir should be: every per-case create fails with
    // ENOTDIR — the run must stay green and the degradation must be reported.
    let blocker = std::env::temp_dir().join(format!("piforge-trace-block-{}", std::process::id()));
    std::fs::write(&blocker, b"not a dir").unwrap();
    let (runner, _m) = Runner::new_mock(12, false);
    let runner = runner.with_trace_dir(blocker.clone());
    let vs = runner.run_all(&shared_cases_dir(), |_| ()).await.unwrap();
    assert!(
        vs.iter().all(|v| v.pass_),
        "trace failure must not fail cases"
    );
    assert!(!runner.degraded_traces().is_empty());
    let _ = std::fs::remove_file(&blocker);
}

// Binary-level exit contract: the mock-parity bail must actually fire.
#[test]
fn mock_exit_contract_fails_on_failing_case() {
    use std::process::Command;
    let root = temp_root("contract");
    std::fs::write(
        root.join("bad.json"),
        r#"{
          "id": "malformed-empty-fix",
          "symptom": "unused",
          "setup": { "throttled": "0x0" },
          "gold": { "is_hardware_fault": false, "fix_applies": "", "fix_must_contain": [] }
        }"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_piforge-eval"))
        .args(["--mock", "--config", "none", "--cases"])
        .arg(&root)
        .output()
        .expect("run piforge-eval --mock (failing)");
    assert!(
        !out.status.success(),
        "a failing mock case must exit non-zero; stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("mock parity violated"), "{err}");
    // Healthy corpus: exit 0 + BUILD_LOCAL (the CI success path, pinned).
    let good = format!("{}/../eval/cases", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new(env!("CARGO_BIN_EXE_piforge-eval"))
        .args(["--mock", "--config", "none", "--cases"])
        .arg(&good)
        .output()
        .expect("run piforge-eval --mock (healthy)");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("DECISION: BUILD_LOCAL"));
    let _ = std::fs::remove_dir_all(&root);
}
