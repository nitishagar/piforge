//! piforge — the interactive agent. Points at a local llama-server.
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use piforge::{agent::Agent, broker::Broker, config, hil::ToolVec, provider::Client};
#[cfg(feature = "hw")]
use piforge::{broker::ThrottledReader, hil_hw};
// `sim` is referenced only by the sim (default) tool-box helper below.
#[cfg(not(feature = "hw"))]
use piforge::sim;

#[derive(Parser)]
#[command(
    name = "piforge",
    about = "The agent that understands live hardware state"
)]
struct Args {
    /// Path to piforge.toml ("" or "none" for defaults).
    #[arg(long, default_value = "piforge.toml")]
    config: String,
    /// Single task to run (non-interactive).
    #[arg(long)]
    task: Option<String>,
    /// Execute one HIL tool and print JSON (skips the LLM). For operator /
    /// Class-R checks. Unknown names error; hardware resolve still runs first.
    #[arg(long)]
    tool: Option<String>,
    /// JSON object of arguments for `--tool`. Default empty object.
    #[arg(long, default_value = "{}")]
    args: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = config::load(&args.config)?;
    cfg.validate()?;

    if args.task.is_some() && args.tool.is_some() {
        anyhow::bail!("--task and --tool are mutually exclusive");
    }
    // I19: clap does not mark --task required; missing both still bails here.
    if args.task.is_none() && args.tool.is_none() {
        anyhow::bail!("no --task given; interactive mode not yet implemented");
    }

    // Resolve the toolbox (chip + i2c bus on hw) before talking to the LLM so a
    // missing `/dev/i2c-1` is a pre-loop error listing available buses (I14).
    let tools: ToolVec = build_tools(&cfg, Broker::stdin_confirmer())?;

    if let Some(name) = args.tool {
        let tool = tools
            .iter()
            .find(|t| t.name() == name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool {name}"))?;
        let parsed: serde_json::Value = serde_json::from_str(&args.args)
            .map_err(|e| anyhow::anyhow!("--args must be a JSON object: {e}"))?;
        let result = tool.execute(&parsed).await;
        println!("{}", result.to_json_string());
        if !result.ok {
            anyhow::bail!("{name} returned ok=false");
        }
        return Ok(());
    }

    let client = Arc::new(Client::new(&cfg.server)?);
    client.health_check().await.map_err(|e| {
        anyhow::anyhow!(
            "{e}\n  (check server.base_url in piforge.toml, and PIFORGE_API_KEY for a cloud provider)"
        )
    })?;

    let agent = Agent::new(
        client,
        tools,
        cfg.agent.max_turns,
        cfg.agent.telemetry_preload,
    );
    let task = args.task.expect("checked above");
    let res = agent.run(&task, |s| println!("{s}")).await?;
    eprintln!(
        "\n[turns={} cache_hit={:.0}% prompt_tok={} completion_tok={}]",
        res.turns,
        res.cache_hit_rate() * 100.0,
        res.prompt_tokens,
        res.completion
    );
    Ok(())
}

/// Build the agent's tool-box. Feature-selected so the binary stays thin under
/// both feature configs: `hil_hw` is whole-module-gated (`#![cfg(feature="hw")]`),
/// so a sim build has no `hil_hw` symbols — a runtime `cfg!` branch would not
/// compile. Both arms return the same 6 tools in the same order.
#[cfg(feature = "hw")]
fn build_tools(
    cfg: &config::Config,
    confirm: Arc<dyn Fn(&str) -> bool + Send + Sync>,
) -> Result<ToolVec> {
    // resolve_chip failure surfaces to the user (no gpiochip => nothing works on
    // the hw path); exit cleanly, do not fall back to sim silently.
    let chip = hil_hw::resolve_chip(&cfg.hardware.gpiochip)
        .map_err(|e| anyhow::anyhow!("no gpiochip found: {e}; set hardware.gpiochip"))?;
    let i2c_bus =
        hil_hw::resolve_i2c_bus(&cfg.hardware.i2c_bus).map_err(|e| anyhow::anyhow!("{e}"))?;
    // Single shared TelemetryTool: one instance backs both the Broker's
    // under-voltage STOP check and the agent-facing `telemetry` tool, so the STOP
    // reflects the same real vcgencmd read the agent sees.
    let telemetry = hil_hw::TelemetryTool::new();
    let gate = Arc::new(Broker::new(
        cfg.safety.clone(),
        Some(telemetry.clone() as Arc<dyn ThrottledReader>),
        confirm,
    ));
    Ok(hil_hw::build_tools(
        &chip,
        i2c_bus.to_string_lossy().as_ref(),
        gate,
        telemetry,
    ))
}

#[cfg(not(feature = "hw"))]
fn build_tools(
    cfg: &config::Config,
    confirm: Arc<dyn Fn(&str) -> bool + Send + Sync>,
) -> Result<ToolVec> {
    // Sim-backed stubs (macOS dev/eval loop). The 6-tool set + order mirrors the
    // hw build exactly — verified by tests/tool_parity.rs.
    let st = sim::State::from_setup(sim::Setup::default());
    let gate = Arc::new(Broker::new(cfg.safety.clone(), Some(st.clone()), confirm));
    Ok(vec![
        sim::InventoryTool::new(st.clone()),
        sim::TelemetryTool::new(st.clone()),
        sim::I2CTool::new(st.clone()),
        sim::GPIOTool::new(st.clone(), Some(gate)),
        sim::ScopeTool::new(st.clone()),
        sim::CodeEditTool::new("."),
    ])
}
