//! piforge-eval — the capability eval gate. --mock for headless CI (no
//! llama-server); otherwise points at a real llama-server.
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use piforge::{
    agent::LlmClient,
    config,
    eval::{decide, summarize, Runner},
    provider::Client,
};

#[derive(Parser)]
#[command(name = "piforge-eval", about = "Capability eval gate")]
struct Args {
    #[arg(long, default_value = "piforge.toml")]
    config: String,
    #[arg(long)]
    cases: Option<String>,
    /// Use scripted mock provider (CI; no llama-server).
    #[arg(long)]
    mock: bool,
    /// Write one JSONL trace per case under DIR/<run-id>/. Local run artifact;
    /// the trace records the run lifecycle (turns, tool calls, results, errors).
    #[arg(long)]
    trace_dir: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut cfg = config::load(&args.config)?;
    if let Some(c) = args.cases {
        cfg.eval.cases_dir = c;
    }

    let mut runner = if args.mock {
        eprintln!("piforge-eval: MOCK mode (scripted provider, no model)");
        let (r, _mock) = Runner::new_mock(cfg.agent.max_turns, cfg.agent.telemetry_preload);
        r
    } else {
        let client = Arc::new(Client::new(&cfg.server)?);
        client.health_check().await?;
        Runner::new(
            client as Arc<dyn LlmClient>,
            cfg.agent.max_turns,
            cfg.agent.telemetry_preload,
        )
    };
    if let Some(dir) = &args.trace_dir {
        use std::time::{SystemTime, UNIX_EPOCH};
        let run_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let run_dir = dir.join(format!("run-{run_id}"));
        std::fs::create_dir_all(&run_dir)?;
        eprintln!("piforge-eval: tracing to {}", run_dir.display());
        runner = runner.with_trace_dir(run_dir);
    }

    eprintln!("piforge-eval: running cases from {}", cfg.eval.cases_dir);
    let verdicts = runner
        .run_all(&cfg.eval.cases_dir, |v| {
            let status = if v.pass_ { "PASS" } else { "FAIL" };
            let extra = if v.hallucination { " [HALLUC]" } else { "" };
            let notes: String = v.notes.chars().take_while(|&c| c != '\n').collect();
            eprintln!(
                "  {:32} {status}{extra}  turns={}  {notes}",
                v.case_id, v.turns
            );
        })
        .await?;

    if verdicts.is_empty() {
        anyhow::bail!(
            "no cases found — add fixtures under {}/*.json",
            cfg.eval.cases_dir
        );
    }

    let s = summarize(&verdicts);
    let decision = decide(
        &s,
        cfg.eval.pass_rate_threshold,
        cfg.eval.hallucination_threshold,
    );
    println!("\n=== Summary ===");
    println!(
        "cases={} passed={} errored={} non_converged={} partial={} hallucinated={}",
        s.total, s.passed, s.errored, s.non_converged, s.partial, s.hallucinated
    );
    println!(
        "pass_rate={:.0}% halluc_rate={:.0}% median_turns={} mean_cache_hit={:.0}%",
        s.pass_rate * 100.0,
        s.hallucination_rate * 100.0,
        s.median_turns,
        s.mean_cache_hit_rate * 100.0
    );
    println!(
        "threshold: pass>={:.0}% halluc<={:.0}%",
        cfg.eval.pass_rate_threshold * 100.0,
        cfg.eval.hallucination_threshold * 100.0
    );
    println!("DECISION: {decision}");
    for case in runner.degraded_traces() {
        eprintln!("trace degraded for {case} (write failure; run unaffected)");
    }
    if args.mock {
        // Mock scripts hit gold by construction: any failure or a non-BUILD
        // decision is a harness regression and must fail CI (exit contract).
        if verdicts.iter().any(|v| !v.pass_) || decision != "BUILD_LOCAL" {
            anyhow::bail!("mock parity violated: every case must pass and decide BUILD_LOCAL");
        }
    }
    Ok(())
}
