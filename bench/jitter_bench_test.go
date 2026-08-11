// Scope-jitter micro-benchmark. This is the moat metric: the worst-case
// inter-sample latency on the edge-event handler loop.
//
// Two variants, for an honest Go-vs-Rust comparison:
//   - BenchmarkScopeJitterForcedGC: forces runtime.GC() every 50 samples
//     (adversarial conditioning to surface GC pause cost). This was the
//     original baseline and OVERSTATES Go's real-world jitter.
//   - BenchmarkScopeJitterDemandGC: the honest, non-adversarial variant. No
//     forced GC; the runtime collects on demand as it would in the real agent
//     loop. Same 1KB allocation every 50 samples to keep the allocator hot
//     and produce realistic GC pressure.
//
// Run: go test -bench=. -benchtime=10x ./bench/
//
// The bench spins a goroutine that fires edges at a fixed cadence; the handler
// records arrival times and reports p50/p99/max jitter vs the cadence.
package bench

import (
	"runtime"
	"sort"
	"testing"
	"time"
)

// BenchmarkScopeJitterForcedGC is the ADVERSARIAL variant (forces GC). Kept
// for reference; prefer DemandGC for an honest comparison.
func BenchmarkScopeJitterForcedGC(b *testing.B) {
	for i := 0; i < b.N; i++ {
		measureJitter(b, 2*time.Second, time.Millisecond, true)
	}
}

// BenchmarkScopeJitterDemandGC is the HONEST variant: no forced GC. The
// runtime collects on demand, as it would in the real agent loop.
func BenchmarkScopeJitterDemandGC(b *testing.B) {
	for i := 0; i < b.N; i++ {
		measureJitter(b, 2*time.Second, time.Millisecond, false)
	}
}

// measureJitter fires edges at the given cadence for dur, recording the
// arrival-time delta vs the expected cadence. When forceGC is true, a GC is
// forced every 50 samples (adversarial); otherwise allocation happens but GC
// runs only on demand (the realistic Go agent-loop behavior).
func measureJitter(b *testing.B, dur, cadence time.Duration, forceGC bool) {
	const expected = 1000 // 1ms in microseconds
	n := int(dur / cadence)
	deltas := make([]int, 0, n)
	arrivals := make(chan time.Time, n)

	// Edge producer: fires at fixed cadence.
	go func() {
		for i := 0; i < n; i++ {
			arrivals <- time.Now()
			time.Sleep(cadence)
		}
		close(arrivals)
	}()

	// Handler: records arrival; allocates to keep the allocator hot.
	prev := time.Now()
	i := 0
	for a := range arrivals {
		delta := int(a.Sub(prev).Microseconds())
		if i > 0 { // skip first (no baseline)
			deltas = append(deltas, delta)
		}
		prev = a
		i++
		// Same 1KB allocation every 50 samples as the Rust bench, to keep the
		// allocator hot and produce realistic GC pressure.
		if i%50 == 0 {
			_ = make([]byte, 1024)
			if forceGC {
				runtime.GC()
			}
		}
	}

	sort.Ints(deltas)
	p50 := deltas[len(deltas)/2]
	p99 := deltas[len(deltas)*99/100]
	maxD := deltas[len(deltas)-1]
	mode := "demand"
	if forceGC {
		mode = "forced"
	}
	b.Logf("gc=%s n=%d cadence=%v  delta_us p50=%d p99=%d max=%d  (expected=%d, p99_overrun=%.1fx)",
		mode, len(deltas), cadence, p50, p99, maxD, expected, float64(p99)/float64(expected))
}
