//! piforge — the interactive agent. Points at a local llama-server.
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use piforge::{agent::Agent, broker::Broker, config, hil::ToolVec, provider::Client, sim};

#[derive(Parser)]
#[command(name = "piforge", about = "The agent that understands live hardware state")]
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
    client.health_check().await.map_err(|e| anyhow::anyhow!("{e}\nstart llama-server first, e.g.:\n  llama-server -m <qwen3-4b-instruct-2507-q4_k_m.gguf> --jinja --port 8080 -c {} -ctk q8_0 -ctv q8_0", cfg.model.context))?;

    // NOTE: real hardware tools (gpio/i2c/scope) live behind the `hw` feature
    // on Linux. In this dev build they fall back to sim-backed stubs so the
    // loop runs; on the Pi, build with `--features hw`.
    let st = sim::State::from_setup(sim::Setup::default());
    let gate = Arc::new(Broker::new(cfg.safety.clone(), Some(st.clone()), Broker::always_deny()));
    let tools: ToolVec = vec![
        sim::InventoryTool::new(st.clone()),
        sim::TelemetryTool::new(st.clone()),
        sim::I2CTool::new(st.clone()),
        sim::GPIOTool::new(st.clone(), Some(gate)),
        sim::ScopeTool::new(st.clone()),
        sim::CodeEditTool::new("."),
    ];

    let agent = Agent::new(client, tools, cfg.agent.max_turns);
    let task = match args.task {
        Some(t) => t,
        None => anyhow::bail!("no --task given; interactive mode not yet implemented"),
    };
    let res = agent.run(&task, |s| println!("{s}")).await?;
    eprintln!("\n[turns={} cache_hit={:.0}% prompt_tok={} completion_tok={}]",
        res.turns, res.cache_hit_rate() * 100.0, res.prompt_tokens, res.completion);
    Ok(())
}
