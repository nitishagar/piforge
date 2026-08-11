//! Turn-exhaustion / non-converged detection (IMPLICIT_SPEC invariant 6 edge +
//! PLAN Approach §5).
//!
//! A model that loops to `max_turns` without a terminal text response must be
//! reported as `non_converged`, NOT silently folded into `pass=false`. The
//! typed `AgentError::TurnBudgetExhausted` distinguishes "too weak / context too
//! short" from "answered wrong" so the eval gate's verdict is not confounded.
use piforge::agent::{Agent, AgentError, LlmClient};
use piforge::eval::{decide, model_too_weak_banner, summarize, Summary, Verdict};
use piforge::provider::{ChatRequest, ChatResponse, ToolCall};

use async_trait::async_trait;
use std::sync::Arc;

/// A provider that ALWAYS returns a tool call and never a terminal text turn —
/// the agent can never converge.
struct LoopingProvider;
#[async_trait]
impl LlmClient for LoopingProvider {
    async fn chat(&self, _req: &ChatRequest) -> anyhow::Result<ChatResponse> {
        Ok(ChatResponse {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "loop-1".into(),
                kind: "function".into(),
                function: piforge::provider::ToolCallFunction {
                    name: "hardware_inventory".into(),
                    arguments: "{}".into(),
                },
            }],
            finish_reason: "tool_calls".into(),
            prompt_tokens: 100,
            completion: 10,
            cached: 0,
        })
    }
}

#[tokio::test]
async fn looping_model_exhausts_turn_budget() {
    // No tools are wired here, so the dispatched tool result is an "unknown
    // tool" string; the point is the agent never gets a terminal text turn and
    // must return TurnBudgetExhausted, not an arbitrary anyhow error.
    let agent = Agent::new(
        Arc::new(LoopingProvider) as Arc<dyn LlmClient>,
        vec![],
        5, // small budget so the test is fast
    );
    let res = agent.run("diagnose my sensor", |_| ()).await;
    let err = match res {
        Ok(_) => panic!("looping provider must not converge, but run succeeded"),
        Err(e) => e,
    };
    assert!(
        matches!(err, AgentError::TurnBudgetExhausted),
        "expected TurnBudgetExhausted, got {err:?}"
    );
}

#[tokio::test]
async fn chat_failure_is_distinct_from_turn_exhaustion() {
    /// A provider that fails every chat() call — simulating a provider/network
    /// error, NOT turn-budget exhaustion.
    struct FailingProvider;
    #[async_trait]
    impl LlmClient for FailingProvider {
        async fn chat(&self, _req: &ChatRequest) -> anyhow::Result<ChatResponse> {
            Err(anyhow::anyhow!("connection refused"))
        }
    }
    let agent = Agent::new(Arc::new(FailingProvider) as Arc<dyn LlmClient>, vec![], 5);
    let err = match agent.run("diagnose my sensor", |_| ()).await {
        Ok(_) => panic!("failing provider must error, but run succeeded"),
        Err(e) => e,
    };
    assert!(
        matches!(err, AgentError::Chat { .. }),
        "expected AgentError::Chat, got {err:?}"
    );
}

#[test]
fn non_converged_is_reported_separately_in_summary() {
    // A run where half the cases didn't converge: the summary must surface the
    // non_converged count, and decide()'s BUILD/PIVOT/INCONCLUSIVE mapping is
    // UNCHANGED (Approach §5: report alongside, do not remap).
    let vs = vec![
        Verdict {
            case_id: "a".into(),
            pass_: true,
            ..Default::default()
        },
        Verdict {
            case_id: "b".into(),
            non_converged: true, // too weak, not wrong
            ..Default::default()
        },
        Verdict {
            case_id: "c".into(),
            non_converged: true,
            ..Default::default()
        },
    ];
    let s = summarize(&vs);
    assert_eq!(s.total, 3);
    assert_eq!(s.passed, 1);
    assert_eq!(s.non_converged, 2);
    // pass_rate counts only passes; non-convergence does not inflate it.
    assert!((s.pass_rate - (1.0 / 3.0)).abs() < 0.01);

    // decide() mapping is unchanged: pass_rate 0.33 < 0.55 and >= 0.30 →
    // INCONCLUSIVE (not PIVOT — the model isn't catastrophically wrong, it's
    // just not converging on some cases).
    assert_eq!(decide(&s, 0.55, 0.10), "INCONCLUSIVE");
}

#[test]
fn majority_non_converged_triggers_unreliable_threshold() {
    // Exercises the REAL `model_too_weak_banner` predicate the binary prints,
    // not a local copy — so a regression in either the predicate or the binary's
    // use of it is caught. (The binary imports + calls this exact fn.)
    let mk = |non_conv: usize, total: usize| Summary {
        total,
        non_converged: non_conv,
        ..Default::default()
    };
    assert!(!model_too_weak_banner(&mk(0, 3)), "0/3 must not flag");
    assert!(!model_too_weak_banner(&mk(1, 3)), "1/3 must not flag");
    assert!(model_too_weak_banner(&mk(2, 3)), "2/3 must flag");
    assert!(model_too_weak_banner(&mk(1, 1)), "1/1 must flag");
    // exactly half must NOT flag (strict >).
    assert!(!model_too_weak_banner(&mk(1, 2)), "1/2 must not flag");
    // total == 0 must not flag (division guard, never panics).
    assert!(!model_too_weak_banner(&mk(0, 0)), "0/0 must not flag");
}
