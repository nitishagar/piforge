// Scope-jitter micro-benchmark. This is the moat metric: the worst-case
// inter-sample latency on the edge-event handler loop. GC pauses (Go) are the
// suspected enemy; Rust (no GC) should produce a tighter tail.
//
// Run: go test -bench=. -benchtime=5s ./bench/
//
// The bench spins a goroutine that "fires edges" at a fixed cadence; the
// handler records arrival times and reports p50/p99/max jitter vs the cadence.
// On darwin (no real hardware) it still exercises the runtime's scheduling +
// GC behavior, which is what we're measuring.
package bench

import (
	"runtime"
	"sort"
	"testing"
	"time"
)

// BenchmarkScopeJitter measures the tail latency of an edge-event handler
// loop over a 2-second capture window with 1ms cadence (1000 edges).
func BenchmarkScopeJitter(b *testing.B) {
	for i := 0; i < b.N; i++ {
		measureJitter(b, 2*time.Second, time.Millisecond)
	}
}

// measureJitter fires edges at the given cadence for dur, recording the
// arrival-time delta vs the expected cadence. Reports p50/p99/max.
func measureJitter(b *testing.B, dur, cadence time.Duration) {
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

	// Handler: records arrival, forces GC periodically to surface pauses.
	prev := time.Now()
	i := 0
	for a := range arrivals {
		delta := int(a.Sub(prev).Microseconds())
		if i > 0 { // skip first (no baseline)
			deltas = append(deltas, delta)
		}
		prev = a
		i++
		// Force a GC every 50 samples to surface pause impact (the Rust impl
		// has no GC; this isolates Go's cost).
		if i%50 == 0 {
			runtime.GC()
		}
	}

	sort.Ints(deltas)
	p50 := deltas[len(deltas)/2]
	p99 := deltas[len(deltas)*99/100]
	maxD := deltas[len(deltas)-1]
	b.Logf("n=%d cadence=%v  delta_us p50=%d p99=%d max=%d  (expected=%d, p99_overrun=%.1fx)",
		len(deltas), cadence, p50, p99, maxD, expected, float64(p99)/float64(expected))
}
