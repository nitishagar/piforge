# Rust baseline (Pi 5)

Captured for the hardware-fault-clearance harness. Historical mock-eval
`pass_rate=23%` was **pre-script-gap** (only 3 of 13 fixtures had mock scripts).

## Binary

```
~2.7 MB  rust/target/release/piforge       (hw feature, aarch64)
~2.7 MB  rust/target/release/piforge-eval
```

gnu target on the Pi is dynamically linked.

## Tests

```
cargo test
cargo test --features hw
```

## Mock eval timing (historical, pre-script-gap)

```
real 0.56
            ~3.2 MB  peak RSS (VmHWM)
pass_rate=23% halluc_rate=0% median_turns=1 mean_cache_hit=20%
threshold: pass>=55% halluc<=10%
DECISION: PIVOT_TO_CLOUD
```

`passed=3` of `cases=13` because only three mock scripts existed. Not a
current 13/13 capability claim.

## Scope jitter (Rust, Pi 5)

The moat metric: tail latency on the edge-event handler loop at 1 ms cadence.
`cargo bench --bench jitter`; 10× 2 s windows.

```
p99 overrun ≈ 1.1× cadence
max spike   up to 5.2 ms
```
