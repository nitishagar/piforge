//! Needle spike criterion — an EXECUTABLE check (not a prose rubric).
//!
//! This test is `#[ignore]` by default because it consumes the spike's raw
//! output (`needle_raw_output.json`), which only exists after
//! `cactus run Cactus-Compute/needle` is executed on hallpi (Phase 2's manual
//! step). Run it explicitly after the spike:
//!
//!     cargo test --test criterion -- --ignored
//!
//! ## Why executable (anti-gameability)
//!
//! The plan's invariant 12 requires the GO/NO-GO criterion to be declared
//! BEFORE the spike runs. A prose rubric can be re-read leniently after the
//! fact; this executable check cannot — loosening `expected.json` requires a
//! new commit with a visible diff, and the spike report records the SHA of
//! `expected.json` at run time. The criterion:
//!
//!   GO iff Needle dispatches piforge's 6 tools with schema-valid args on
//!   >=8/10 prompts AND hallucinates zero non-existent tools.
//!
//! ## Input format — `needle_raw_output.json`
//!
//! A JSON array, one object per prompt, in `prompts.jsonl` order:
//!
//! ```jsonc
//! [
//!   {
//!     "prompt_id": "p1",
//!     "dispatched_tool": "hardware_inventory",   // null/missing = no dispatch
//!     "tool_args": {},                            // the args Needle produced
//!     "hallucinated_tools": []                    // any non-piforge tool names
//!   },
//!   ...
//! ]
//! ```
//!
//! ## Verdict
//!
//! Prints a dispatch matrix and a binary `NEEDLE_SPIKE: GO` or `NEEDLE_SPIKE: NO-GO`.
//! The test PASSES (returns Ok) as long as it could run the criterion and print a
//! verdict; the GO/NO-GO is an artifact for the report, not a CI pass/fail — the
//! spike is research, and a NO-GO is a valid finding, not a test failure.
use serde::Deserialize;
use std::collections::HashSet;

#[derive(Debug, Deserialize)]
struct ExpectedSpec {
    thresholds: Thresholds,
    tools_in_scope: Vec<String>,
    prompts: std::collections::HashMap<String, ExpectedPrompt>,
}
#[derive(Debug, Deserialize)]
struct Thresholds {
    min_schema_valid_dispatches: usize,
    max_hallucinated_tools: usize,
    total_prompts: usize,
}
#[derive(Debug, Deserialize)]
struct ExpectedPrompt {
    expected_tool: String,
    #[serde(default)]
    required_args: serde_json::Value,
}
#[derive(Debug, Deserialize)]
struct RawResult {
    prompt_id: String,
    dispatched_tool: Option<String>,
    #[serde(default)]
    tool_args: serde_json::Value,
    #[serde(default)]
    hallucinated_tools: Vec<String>,
}

/// Read a sibling file from this test's directory (next to criterion.rs).
fn read_sibling(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/needle_probe")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
#[ignore = "Needs needle_raw_output.json from the hallpi spike run; invoke with --ignored"]
fn needle_spike_criterion() {
    let spec: ExpectedSpec =
        serde_json::from_str(&read_sibling("expected.json")).expect("parse expected.json");
    let raw: Vec<RawResult> =
        serde_json::from_str(&read_sibling("needle_raw_output.json")).expect("parse raw output");

    let tools_in_scope: HashSet<String> = spec.tools_in_scope.iter().cloned().collect();
    assert_eq!(
        spec.thresholds.total_prompts,
        spec.prompts.len(),
        "expected.json: total_prompts {} != prompts.len() {}",
        spec.thresholds.total_prompts,
        spec.prompts.len()
    );

    let mut schema_valid = 0usize;
    let mut total_hallucinated = 0usize;
    println!("\n=== Needle spike dispatch matrix ===");
    println!(
        "{:<6} {:<20} {:<20} {:<8}",
        "id", "expected", "dispatched", "valid?"
    );
    println!("{}", "-".repeat(60));
    for r in &raw {
        let exp = spec
            .prompts
            .get(&r.prompt_id)
            .unwrap_or_else(|| panic!("raw prompt_id {} not in expected.json", r.prompt_id));
        let dispatched = r.dispatched_tool.as_deref().unwrap_or("(none)");
        let mut valid = dispatched == exp.expected_tool;
        if valid {
            // Args schema check: every required key in expected must be present
            // AND equal in the dispatched args (a subset match — Needle may add
            // extra keys, but the required ones must be right).
            if let Some(req) = exp.required_args.as_object() {
                if let Some(got) = r.tool_args.as_object() {
                    for (k, v) in req {
                        if got.get(k) != Some(v) {
                            valid = false;
                            break;
                        }
                    }
                } else {
                    valid = false;
                }
            }
        }
        if valid {
            schema_valid += 1;
        }
        // Hallucinated tools = any dispatched tool NOT in the piforge scope.
        let halluc: Vec<&str> = r
            .hallucinated_tools
            .iter()
            .filter(|t| !tools_in_scope.contains(*t))
            .map(String::as_str)
            .collect();
        total_hallucinated += halluc.len();
        println!(
            "{:<6} {:<20} {:<20} {:<8} {}",
            r.prompt_id,
            exp.expected_tool,
            dispatched,
            if valid { "yes" } else { "NO" },
            if halluc.is_empty() {
                String::new()
            } else {
                format!("halluc: {}", halluc.join(","))
            }
        );
    }

    let go = schema_valid >= spec.thresholds.min_schema_valid_dispatches
        && total_hallucinated <= spec.thresholds.max_hallucinated_tools;
    println!("{}", "-".repeat(60));
    println!(
        "schema_valid_dispatches = {} / {} (need >= {})",
        schema_valid,
        raw.len(),
        spec.thresholds.min_schema_valid_dispatches
    );
    println!(
        "hallucinated_tools     = {} (max {})",
        total_hallucinated, spec.thresholds.max_hallucinated_tools
    );
    println!("NEEDLE_SPIKE: {}", if go { "GO" } else { "NO-GO" });
    // The test passes regardless of GO/NO-GO — the verdict is the artifact.
    // A NO-GO is a valid finding (Needle is too weak), not a CI failure.
}

#[test]
fn expected_json_is_self_consistent() {
    // This NON-ignored test runs in CI to catch regressions in the pre-declared
    // criterion itself (a typo'd expected.json would silently break the spike).
    let spec: ExpectedSpec =
        serde_json::from_str(&read_sibling("expected.json")).expect("parse expected.json");
    assert_eq!(spec.thresholds.total_prompts, 10, "10 prompts expected");
    assert_eq!(spec.prompts.len(), 10, "10 prompt entries expected");
    assert_eq!(spec.tools_in_scope.len(), 6, "6 piforge tools expected");
    // Every expected_tool must be in scope.
    for (id, p) in &spec.prompts {
        assert!(
            spec.tools_in_scope.contains(&p.expected_tool),
            "{id}: expected_tool {} not in tools_in_scope",
            p.expected_tool
        );
    }
    // The prompts.jsonl must have the same prompt ids.
    let raw = read_sibling("prompts.jsonl");
    let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 10, "prompts.jsonl should have 10 lines");
    let jsonl_ids: HashSet<String> = lines
        .iter()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .unwrap_or_else(|e| panic!("prompts.jsonl bad line: {e}"))
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string()
        })
        .collect();
    let expected_ids: HashSet<String> = spec.prompts.keys().cloned().collect();
    assert_eq!(
        jsonl_ids, expected_ids,
        "prompts.jsonl ids != expected.json ids"
    );
}
