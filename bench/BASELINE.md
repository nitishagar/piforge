# Go baseline (main)

Captured before the Rust parallel rewrite. The Rust branch must beat these
numbers (RSS, binary, scope jitter) to justify a merge.

## Binary

```
6357176 dist/piforge-arm64
6226104 dist/piforge-eval-arm64
piforge-arm64: statically linked
```

## Tests
```
ok  	github.com/nitishagar/piforge/internal/broker	(cached)
ok  	github.com/nitishagar/piforge/internal/eval	(cached)
ok  	github.com/nitishagar/piforge/internal/hil	(cached)
ok  	github.com/nitishagar/piforge/internal/sim	(cached)
```

## Mock eval timing (end-to-end harness)
```
real 0.56
            12255232  maximum resident set size
pass_rate=23% halluc_rate=0% median_turns=1 mean_cache_hit=20%
threshold: pass>=55% halluc<=10%
DECISION: PIVOT_TO_CLOUD
```

## Scope jitter (Go, with forced GC every 50 samples)

The moat metric: tail latency on the edge-event handler loop at 1ms cadence.
Go's GC pauses are the suspected enemy. Captured on the dev host (Apple M4 Pro,
darwin/arm64); real-Pi numbers will differ in absolute terms but the GC-pause
signature should hold.

The bench forces a GC every 50 samples to surface pause impact (the Rust impl
has no GC). p99 overrun of ~1.2-1.4x and max ~2.7ms is the target to beat.

```
cpu: Apple M4 Pro
BenchmarkScopeJitter-14    2    2275679416 ns/op
delta_us p50=1139 p99=1406 max=2701  (expected=1000, p99_overrun=1.4x)
delta_us p50=1137 p99=1216 max=1692  (expected=1000, p99_overrun=1.2x)
delta_us p50=1139 p99=1223 max=1677  (expected=1000, p99_overrun=1.2x)
```

## Verdict criteria (for the Rust branch)

Rust wins the rewrite if it beats the Go baseline on:
  - binary size:   < 6.3 MB (Go: 6.36 MB)
  - RSS:           < 12 MB  (Go: 12.26 MB peak)
  - scope p99 jitter: < 1.2x cadence (Go: 1.2-1.4x with forced GC)
  - tests + eval parity: same pass/halluc decisions on the same fixtures
