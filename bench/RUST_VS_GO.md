# Rust bench notes (size, RSS, scope jitter)

Pi 5 native aarch64 measurements for the hardware-fault-clearance harness.
Historical mock-eval `pass_rate=23%` (`cases=13 passed=3`) was **pre-script-gap**:
only 3 of 13 fixtures had mock scripts. Do not treat that figure as a current
capability claim.

## Binary and RSS (Pi 5)

| Metric | Value |
|---|---|
| **Binary** | ~2.7 MB (`piforge` with `hw`, and `piforge-eval`) |
| **Peak RSS** (VmHWM, mock eval) | ~3.2 MB |

Harness RSS is rounding noise next to the ~2.5 GB Q4 model + ~1 GB KV cache
the llama-server process holds. Binary size matters for drop-on-Pi delivery.

The gnu `aarch64-unknown-linux-gnu` build is dynamically linked. A musl-static
build is out of scope. The `hw` feature (gpiod/nix) adds negligible size.

RSS: measured via `/proc/<pid>/status VmHWM`. Linux excludes shared/read-only
pages that macOS counts, so absolute numbers are smaller than typical
dev-host figures.

## Scope jitter (Pi 5, 10+ iters)

The moat metric: tail latency of an edge-event handler loop at 1 ms cadence
over a 2 s window (`cargo bench --bench jitter`).

| Variant | p99 overrun | max spike |
|---|---|---|
| Rust (no GC) | **1.1×** | up to 5.2 ms |

Max spikes are likely Linux scheduling preemption on the `spawn_blocking`
thread, not a language-property win to chase.

## Mock eval (historical, pre-script-gap)

```
cases=13 passed=3 partial=2 hallucinated=0
pass_rate=23% halluc_rate=0% median_turns=1 mean_cache_hit=20%
DECISION: PIVOT_TO_CLOUD
```

Only 3 of 13 cases had mock scripts; the remaining 10 were unexercised
no-ops. That 23% is not a 13-fixture capability result.

## What the jitter bench covers

Red-team regression tests cover under-voltage-bypass, symlink-escape, and
`decide` thresholds. The jitter bench uses `sample_size(10)` (criterion
requires ≥10).
