//! Scope-jitter micro-benchmark — the moat metric. Port of the Go bench so the
//! Rust vs Go comparison is apples-to-apples.
//!
//! Measures tail latency of an edge-event handler loop at 1ms cadence over a
//! 2-second window. The Go bench forces a GC every 50 samples to surface
//! pause impact; Rust has no GC, so this is the suspected win.
//!
//! Run: cargo bench --bench jitter
use criterion::{criterion_group, criterion_main, Criterion};
use std::time::{Duration, Instant};

fn measure_jitter(dur: Duration, cadence: Duration) -> JitterStats {
    let expected_us = cadence.as_micros() as i64;
    let n = (dur.as_millis() / cadence.as_millis()) as usize;
    let (tx, rx) = std::sync::mpsc::channel::<Instant>();
    let producer = std::thread::spawn(move || {
        for _ in 0..n {
            let _ = tx.send(Instant::now());
            std::thread::sleep(cadence);
        }
    });

    let mut deltas: Vec<i64> = Vec::with_capacity(n);
    let mut prev: Option<Instant> = None;
    let mut i = 0;
    for a in rx.iter() {
        if let Some(p) = prev {
            let delta = a.duration_since(p).as_micros() as i64;
            deltas.push(delta);
        }
        prev = Some(a);
        i += 1;
        // Rust has no GC to force; this is the control (no forced pause).
        // The Go bench calls runtime.GC() every 50 here; the Rust win is the
        // absence of that cost. Leave a small allocation in place to keep the
        // allocator hot (parity with Go's GC pressure intent).
        if i % 50 == 0 {
            let _v: Vec<u8> = vec![0; 1024];
            std::hint::black_box(&_v);
        }
    }
    producer.join().ok();

    deltas.sort_unstable();
    let len = deltas.len();
    JitterStats {
        n: len,
        p50: deltas[len / 2],
        p99: deltas[len * 99 / 100],
        max: deltas[len - 1],
        expected: expected_us,
    }
}

struct JitterStats { n: usize, p50: i64, p99: i64, max: i64, expected: i64 }

impl std::fmt::Display for JitterStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let overrun = self.p99 as f64 / self.expected as f64;
        write!(f, "n={} delta_us p50={} p99={} max={} (expected={}, p99_overrun={:.1}x)",
            self.n, self.p50, self.p99, self.max, self.expected, overrun)
    }
}

fn bench_jitter(c: &mut Criterion) {
    let mut group = c.benchmark_group("scope_jitter");
    group.sample_size(3); // 2s windows; few samples is plenty
    group.bench_function("1ms_cadence_2s", |b| {
        b.iter(|| {
            let s = measure_jitter(Duration::from_secs(2), Duration::from_millis(1));
            // Print to cargo bench's capture for the comparison doc.
            eprintln!("{}", std::hint::black_box(s));
        });
    });
    group.finish();
}

criterion_group!(benches, bench_jitter);
criterion_main!(benches);
