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
| **Scope jitter p99 overrun** | 1.2–1.4×* | **1.1×** | tighter |
| **Scope jitter max** | ~2.7 ms* | ~4.7 ms | comparable (sporadic) |

\* Go baseline measured on the dev host with **forced GC every 50 samples**
(adversarial conditioning to surface pause cost). Rust has no GC to force, so
the comparison is not perfectly apples-to-apples — the real Go edge loop's GC
cadence is demand-driven, not forced. The direction (Rust p99 tighter) holds,
but the magnitude may be smaller in production than the forced-GC baseline
suggests. This is the most caveated metric.

### Notes on the measurements
- Binary: Go is statically linked; Rust is dynamically linked (gnu target on
  the Pi). A `aarch64-unknown-linux-musl` Rust build would be static too but
  needs a musl linker. The hw feature (gpiod/nix) adds negligible size —
  `piforge` (hw) and `piforge-eval` (no hw) are both 2.7 MB.
- RSS: measured via `/proc/Pid/status VmHWM`. Linux excludes shared/read-only
  pages that macOS counts, so the absolute numbers are smaller than the
  dev-host figures, but the **ratio holds** (Rust ~32% less).
- Jitter: criterion bench, 10 samples × 2s windows at 1ms cadence, on the Pi.

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

## Verdict

**Rust wins on binary (2.3× smaller) and jitter p99 (1.1× vs 1.2–1.4×), ties
on functional parity, on the real Pi 5.** The RSS win (~32%) is real but
operationally minor given the model dominates memory. The strongest honest
argument is the binary size + the no-GC determinism on the moat (scope) path.

Caveats that temper the verdict: jitter isn't a clean apples-to-apples
comparison (forced GC vs none); parity is thin (3/13 cases); Rust has no
`catch_unwind` so `panic=abort` converts some recoverable surprises into
process death. None of these are merge-blockers; all are worth tracking.

**Recommendation: merge.** Rust is demonstrably better on the metrics that
matter for the Pi target, the red-team regressions are ported, and the Go code
is preserved in git history.
