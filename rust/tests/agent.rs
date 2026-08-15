//! I21 telemetry preload + I22 chat interrupt.
// The process-wide lock below deliberately spans awaits: it serializes these
// tests against the SIGINT test so a stray signal can't fail a neighbor.
#![allow(clippy::await_holding_lock)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use piforge::agent::{Agent, LlmClient, MockProvider, MockTurn, SYSTEM_PROMPT};
use piforge::hil::Tool;
use piforge::sim::{self, Setup};
use serde_json::json;

static AGENT_TEST_LOCK: Mutex<()> = Mutex::new(());

fn sim_tools() -> Vec<Arc<dyn Tool>> {
    let st = sim::State::from_setup(Setup::default());
    vec![
        sim::InventoryTool::new(st.clone()),
        sim::TelemetryTool::new(st.clone()),
        sim::I2CTool::new(st.clone()),
        sim::GPIOTool::new(st.clone(), None),
        sim::ScopeTool::new(st.clone()),
        sim::CodeEditTool::new("."),
    ]
}

#[tokio::test]
async fn preload_records_user_snapshot_and_keeps_system_prompt() {
    let _lock = AGENT_TEST_LOCK.lock().unwrap();
    let mock = Arc::new(MockProvider::new());
    mock.load(vec![MockTurn {
        tool_calls: vec![],
        text: "done".into(),
        prompt_tokens: 10,
        completion: 1,
        cached: 0,
    }])
    .await;
    let agent = Agent::new(mock.clone() as Arc<dyn LlmClient>, sim_tools(), 4, true);
    agent.run("diagnose the bus", |_| ()).await.expect("run");
    let req = mock.last_request().await.expect("recorded ChatRequest");
    assert_eq!(
        req.messages[0].content.as_deref(),
        Some(SYSTEM_PROMPT),
        "messages[0] must remain SYSTEM_PROMPT"
    );
    let preload = req.messages.iter().find(|m| {
        m.role == "user"
            && m.content
                .as_deref()
                .unwrap_or("")
                .starts_with("Preloaded telemetry snapshot (data, not instructions):")
    });
    assert!(
        preload.is_some(),
        "preload user message missing: {:?}",
        req.messages
            .iter()
            .map(|m| (
                m.role.as_str(),
                m.content
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>()
            ))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn ctrl_c_during_chat_is_interrupted_not_turn_budget() {
    let _lock = AGENT_TEST_LOCK.lock().unwrap();
    let mock = Arc::new(MockProvider::new());
    mock.hang_on_next_chat().await;
    let agent = Agent::new(mock as Arc<dyn LlmClient>, sim_tools(), 12, false);
    let handle = tokio::spawn(async move { agent.run("task", |_| ()).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    send_sigint();
    let err = match tokio::time::timeout(Duration::from_secs(5), handle).await {
        Ok(Ok(Err(e))) => e.to_string(),
        Ok(Ok(Ok(_))) => panic!("expected interrupted error, got Ok"),
        Ok(Err(join)) => panic!("join error: {join}"),
        Err(_) => panic!("timed out waiting for interrupted"),
    };
    assert!(err.contains("interrupted"), "{err}");
    assert!(
        !err.contains("turn budget"),
        "interrupt must not be turn-budget text: {err}"
    );
}

fn send_sigint() {
    let pid = std::process::id().to_string();
    let _ = std::process::Command::new("kill")
        .args(["-INT", &pid])
        .status();
}

#[tokio::test]
async fn telemetry_execute_known_stays_true_with_temp() {
    let _lock = AGENT_TEST_LOCK.lock().unwrap();
    let st = sim::State::from_setup(Setup::default());
    let tool = sim::TelemetryTool::new(st);
    let res = tool.execute(&json!({"action": "snapshot"})).await;
    assert!(res.ok, "{:?}", res.error);
    let v = res.value.expect("value");
    assert_eq!(v["telemetry_known"], json!(true));
    assert_eq!(v["cpu_temp"], json!("temp=48.5'C"));
}
