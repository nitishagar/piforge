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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = config::load(&args.config)?;
    cfg.validate()?;

    let client = Arc::new(Client::new(&cfg.server)?);
    client.health_check().await.map_err(|e| {
        anyhow::anyhow!(
            "{e}\n  (check server.base_url in piforge.toml, and PIFORGE_API_KEY for a cloud provider)"
        )
    })?;

    // cfg-selected tool construction — two builds, one thin binary. The `hw`
    // build (Linux, `--features hw`) reads live GPIO/I2C/telemetry through the
    // agent loop; the default (sim) build stands in for the macOS dev/eval loop.
    // `confirm` gates Class I (physical) ops — always_deny today, so the agent
    // can read + scope but not drive pins; real interactive confirm is a future
    // plan's scope. See build_tools() below.
    let tools: ToolVec = build_tools(&cfg, Broker::always_deny())?;

    let agent = Agent::new(client, tools, cfg.agent.max_turns);
    let task = match args.task {
        Some(t) => t,
        None => anyhow::bail!("no --task given; interactive mode not yet implemented"),
    };
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
        &cfg.hardware.i2c_bus,
        gate,
        telemetry,
    ))
}

#[cfg(not(feature = "hw"))]
fn build_tools(
    cfg: &config::Config,
    _confirm: Arc<dyn Fn(&str) -> bool + Send + Sync>,
) -> Result<ToolVec> {
    // Sim-backed stubs (macOS dev/eval loop). The 6-tool set + order mirrors the
    // hw build exactly — verified by tests/tool_parity.rs.
    let st = sim::State::from_setup(sim::Setup::default());
    let gate = Arc::new(Broker::new(
        cfg.safety.clone(),
        Some(st.clone()),
        Broker::always_deny(),
    ));
    Ok(vec![
        sim::InventoryTool::new(st.clone()),
        sim::TelemetryTool::new(st.clone()),
        sim::I2CTool::new(st.clone()),
        sim::GPIOTool::new(st.clone(), Some(gate)),
        sim::ScopeTool::new(st.clone()),
        sim::CodeEditTool::new("."),
    ])
}
