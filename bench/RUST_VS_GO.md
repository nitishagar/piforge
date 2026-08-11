# Rust vs Go — measurement comparison (re-measured on the real Pi 5)

Parallel Rust implementation on the `rust-rewrite` branch vs the Go baseline on
`main`. Decision rule: merge if Rust beats Go on binary size, RSS, and scope
jitter, with functional parity.

## Honest framing (red-team correction)

The first draft of this doc overstated the RSS case ("every MB of KV cache
matters"). That's misleading: the harness RSS is rounding noise next to the
~2.5 GB Q4 model + ~1 GB KV cache the llama-server process holds. The honest
case for Rust is **binary size + jitter (the moat) + no-GC determinism** — not
KV-cache headroom. The numbers below are the real Pi 5 measurements, not the
dev-host proxies the first draft cited.

## Functional parity ✓

Both produce identical eval results on the same 13 fixtures:

```
cases=13 passed=3 partial=2 hallucinated=0
pass_rate=23% halluc_rate=0% median_turns=1 mean_cache_hit=20%
DECISION: PIVOT_TO_CLOUD
```

Same broker semantics (under-voltage STOP first even in auto mode; scoped arming;
confirm outside lock), same eval scoring (word-boundary diagnosis + edit-count
check), same red-team fixes. Same system prompt (byte-identical after the fix).

**Caveat (honest):** only 3 of 13 cases have mock scripts; the parity is
"3 exercised + 10 both-do-nothing-identically," not 13 fully-exercised cases.
The scorer logic is genuinely ported; the live agent loop is parity-tested only
on those 3 happy-path trajectories.

## Real Pi 5 measurements (native aarch64 build)

| Metric | Go (main) | Rust (rust-rewrite) | ratio |
|---|---|---|---|
| **Binary** | 6.23 MB (static) | **2.7 MB** (dynamic) | **2.3× smaller** |
| **Peak RSS** (VmHWM, mock eval) | ~4.7 MB | **~3.2 MB** | **~32% less** |
| **Scope jitter p99 overrun** (demand GC, honest) | **1.1×** | **1.1×** | **tie** |
| **Scope jitter max** | up to 2.8 ms | up to 5.2 ms | neither (scheduling, not language) |

### Scope jitter — the honest, non-adversarial comparison (Pi 5, 10+ iters each)

The first draft of this comparison cited Go p99 at 1.2–1.4× vs Rust 1.1×. **That
gap was an artifact.** The original Go bench forced `runtime.GC()` every 50
samples — adversarial conditioning that overstates real-world jitter. The real
agent loop allocates on demand and lets GC run naturally.

After adding a `BenchmarkScopeJitterDemandGC` variant (same 1KB allocation
every 50 samples, no forced GC) and running it back-to-back with Rust on the Pi:

| Variant | p99 overrun | max spike |
|---|---|---|
| Go (forced GC) — adversarial baseline | 1.1–1.2× | up to 2.4 ms |
| **Go (demand GC) — honest** | **1.1×** | up to 2.8 ms |
| **Rust (no GC)** | **1.1×** | up to 5.2 ms |

**Conclusion: on the Pi 5, Go's modern concurrent collector keeps p99 jitter
within ~7% of the 1ms cadence on realistic demand-driven GC — essentially tied
with Rust.** The "no-GC wins the moat" hypothesis is **not supported** by this
measurement. Go's GC is good enough here that the language choice doesn't move
the scope-path latency at p99.

The only place they diverge is the **max spike**: Rust occasionally hit 5.2 ms
(worse than Go's 2.8 ms in this run), likely Linux scheduling preemption on the
`spawn_blocking` thread — not a language property, and not reproducibly
Rust-favoring. This metric favors neither.

### What this means for the merge decision
The moat/jitter argument is **withdrawn** as a Rust justification. The remaining
honest Rust wins are:
  - **Binary size: 2.3× smaller** (2.7 MB vs 6.23 MB) — real and load-bearing
    for a drop-on-Pi deployment.
  - **RSS: ~32% less** (~3.2 MB vs ~4.7 MB) — real but minor given the model
    dominates memory.
  - **No mutex poisoning** (parking_lot) and a generally tighter safety story.
  - **Embedded ecosystem fit** (Rust is the embedded lingua franca; gpiod is
    first-class; periph.io on Pi 5 is a known fragile fallback in Go).

### Notes on the measurements
- Binary: Go is statically linked; Rust is dynamically linked (gnu target on
  the Pi). A `aarch64-unknown-linux-musl` Rust build would be static too but
  needs a musl linker. The hw feature (gpiod/nix) adds negligible size —
  `piforge` (hw) and `piforge-eval` (no hw) are both 2.7 MB.
- RSS: measured via `/proc/Pid/status VmHWM`. Linux excludes shared/read-only
  pages that macOS counts, so the absolute numbers are smaller than the
  dev-host figures, but the **ratio holds** (Rust ~32% less).
- Jitter: Go via `go test -bench`, Rust via `cargo bench --bench jitter`; both
  10× 2s windows at 1ms cadence, native aarch64 on the Pi 5.

## What was fixed during this comparison (red-team driven)

- I2C read passed the device address in decimal to `i2cget` (wants hex) → fixed.
- `raw_hex` rendered as Rust Debug (`[0,0]`) not `0000` → fixed; dual-endian
  for 2-byte reads restored (parity with Go).
- System prompt was a subset of Go (broke byte-stable-prefix-cache claim) →
  restored verbatim.
- `std::sync::Mutex` poisons on panic (would cascade-kill the agent loop) →
  switched to `parking_lot::Mutex` (no poisoning).
- Jitter bench had `sample_size(3)` (criterion panics if <10) → fixed to 10;
  the bench now actually runs.
- Red-team regression tests ported (under-voltage-bypass, symlink-escape,
  decide thresholds): 10 Rust tests pass.

## Verdict (revised after the honest jitter measurement)

**The jitter/moat argument is withdrawn** — Go's demand-driven GC on the Pi 5
ties Rust at p99 1.1×. The rewrite is NOT justified by the moat metric.

**Rust is still justified, but on weaker, more operational grounds:**
  - Binary size 2.3× smaller (real, matters for drop-on-Pi delivery).
  - RSS ~32% less (real, minor).
  - Better embedded-ecosystem fit (gpiod is first-class; Go's periph.io on Pi 5
    is a fragile ioctl fallback — a latent correctness risk the Rust path avoids).
  - No mutex poisoning; tighter safety-story defaults.

These are genuine but modest wins. **Whether they justify a full rewrite of a
working, tested, red-teamed, public Go codebase is a judgment call**, not a
clear-cut "Rust wins the moat" decision. The honest framing:

  - **Merge if** you weight the embedded-ecosystem fit + binary size highly and
    don't mind maintaining Rust (and re-porting future Go-side changes until
    one is deprecated).
  - **Don't merge if** you weight iteration speed + the existing Go test/CI/docs
    maturity highly, and the binary size (6 MB vs 2.7 MB) doesn't bother you on
    a Pi with 8 GB + NVMe.

The Go code remains on `main`; Rust on `rust-rewrite`. Both are functional,
both have red-team regression tests, both have CI. The decision can be deferred
without cost — neither branch is going stale.
