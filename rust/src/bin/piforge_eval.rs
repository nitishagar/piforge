//! piforge-eval — the capability eval gate. --mock for headless CI (no
//! llama-server); otherwise points at a real llama-server.
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use piforge::{
    agent::LlmClient,
    config,
    eval::{decide, model_too_weak_banner, summarize, Runner},
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut cfg = config::load(&args.config)?;
    if let Some(c) = args.cases {
        cfg.eval.cases_dir = c;
    }

    let runner = if args.mock {
        eprintln!("piforge-eval: MOCK mode (scripted provider, no model)");
        let (r, _mock) = Runner::new_mock(cfg.agent.max_turns);
        r
    } else {
        let client = Arc::new(Client::new(&cfg.server)?);
        client.health_check().await?;
        Runner::new(client as Arc<dyn LlmClient>, cfg.agent.max_turns)
    };

    eprintln!("piforge-eval: running cases from {}", cfg.eval.cases_dir);
    let verdicts = runner
        .run_all(&cfg.eval.cases_dir, |v| {
            let status = if v.pass_ {
                "PASS"
            } else if v.non_converged {
                "NONC"
            } else {
                "FAIL"
            };
            let extra = if v.hallucination { " [HALLUC]" } else { "" };
            let notes: String = v.notes.chars().take_while(|&c| c != '\n').collect();
            eprintln!(
                "  {:32} {status}{extra}  turns={}  {notes}",
                v.case_id, v.turns
            );
        })
        .await?;

    if verdicts.is_empty() {
        eprintln!(
            "no cases found — add fixtures under {}/*.json",
            cfg.eval.cases_dir
        );
        return Ok(());
    }

    let s = summarize(&verdicts);
    let decision = decide(
        &s,
        cfg.eval.pass_rate_threshold,
        cfg.eval.hallucination_threshold,
    );
    println!("\n=== Summary ===");
    println!(
        "cases={} passed={} partial={} hallucinated={} non_converged={}",
        s.total, s.passed, s.partial, s.hallucinated, s.non_converged
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
    // Turn-exhaustion banner: a run where most cases didn't converge means the
    // model is too weak / context too short for the decision to be trusted.
    // Reported alongside the decision, NOT folded into it.
    if model_too_weak_banner(&s) {
        println!("MODEL_TOO_WEAK_OR_CONTEXT_TOO_SHORT — verdict unreliable");
    }
    println!("DECISION: {decision}");
    Ok(())
}
