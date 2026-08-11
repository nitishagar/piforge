# Needle spike report — 2026-08-11

**Binary verdict: NO-GO.** Needle (26M tool-calling model, `Cactus-Compute/needle`,
CQ4) is **removed from scope**. It cannot dispatch piforge's 6 tools correctly.

## Pre-declared criterion (committed before the spike ran)
GO iff Needle dispatches piforge's 6 tools with schema-valid args on **≥8/10**
prompts AND hallucinates **zero** non-existent tools. The criterion and the
expected matrix were committed to the repo (`rust/tests/needle_probe/expected.json`
+ `prompts.jsonl` + `needle_criterion.rs`) **before** the spike was run, and the
criterion is an **executable test** (not a prose rubric) so post-hoc loosening
requires a visible commit diff.

## Result — the dispatch matrix (from the executable criterion test)
```
id     expected             dispatched           valid?
------------------------------------------------------------
p1     hardware_inventory   edit_file            NO
p2     telemetry            gpio                 NO
p3     i2c                  edit_file            NO
p4     i2c                  edit_file            NO
p5     gpio                 edit_file            NO
p6     gpio                 edit_file            NO
p7     scope                gpio                 NO
p8     telemetry            edit_file            NO
p9     i2c                  edit_file            NO
p10    edit_file            gpio                 NO
------------------------------------------------------------
schema_valid_dispatches = 0 / 10  (criterion needs >= 8)
hallucinated_tools     = 0        (criterion max 0)
NEEDLE_SPIKE: NO-GO
```

## Why NO-GO
Needle dispatched the **wrong** tool on all 10 prompts (e.g. for "scan the I2C
bus" it dispatched `edit_file`; for "read GPIO pin 17" it dispatched `edit_file`
or `gpio` with `{"action": "one tool call"}`). The arguments were uniformly
garbage (`{'path': 'tool-dispatch.se', 'content': 'Updated tools: Project Request'}`,
`{'action': 'one tool call', 'pin': 'one tool call'}`) — the model emits the
*shape* of a tool call but not a meaningful one. It did not hallucinate
non-piforge tools (0 hallucinated), so it respects the tool list; it simply
cannot map a natural-language request to the correct tool + schema.

A 26M-parameter model is below the floor for semantic tool dispatch over a
6-tool hardware-diagnosis schema. This is the expected outcome for a model this
small; the spike's value is the **measured, pre-declared** binary, not a guess.

## Decision (per the plan's invariant 12)
**NO-GO ⇒ Needle is removed from scope.** No Needle integration work proceeds
(no FFI, no `cactus serve` Needle path in the interactive binary). The eval
gate runs on Qwen3 (see `REAL_EVAL_2026-08-11.md`); the interactive binary
(Plan A) proceeds without a Needle tool-dispatch accelerator. If a future,
larger tool-calling model is evaluated, the same pre-declared criterion +
`needle_criterion.rs` harness can be reused.

## Reproducibility
- Needle served via `cactus serve` on hallpi (Pi 5): `needle-cq4` bundle.
- 10 prompts from `rust/tests/needle_probe/prompts.jsonl` sent with piforge's 6
  tool definitions; raw dispatches captured to `needle_raw_output.json`.
- Verdict from the executable criterion: `cargo test --test needle_criterion
  -- --include-ignored --nocapture` (the `#[ignore]` test reads
  `needle_raw_output.json` and prints the matrix + GO/NO-GO).
