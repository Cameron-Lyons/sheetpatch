//! Calibrated wall-time measurements, including destruction of returned values.

use std::time::{Duration, Instant};
use std::{env, hint};

pub struct Measurement {
    pub iterations: u32,
    pub min: Duration,
    pub median: Duration,
    pub p90: Duration,
    pub max: Duration,
}

fn batch<T>(iterations: u32, operation: &mut impl FnMut() -> T) -> Duration {
    let started = Instant::now();
    for _ in 0..iterations {
        // Include freeing results: retaining a whole batch would distort memory
        // use, and dropping outside the timer hides part of the operation's cost.
        drop(hint::black_box(operation()));
    }
    started.elapsed()
}

pub fn measure<T>(samples: usize, mut operation: impl FnMut() -> T) -> Measurement {
    let milliseconds = env::var("SHEETPATCH_BENCH_SAMPLE_MS")
        .map(|value| {
            value
                .parse::<u64>()
                .expect("SHEETPATCH_BENCH_SAMPLE_MS must be a nonnegative integer")
        })
        .unwrap_or(50);
    let target = Duration::from_millis(milliseconds);
    let mut iterations = 1u32;
    // Calibration also warms the operation. Zero selects one iteration for
    // quick CI correctness runs, with no separate calibration invocation.
    if !target.is_zero() {
        while batch(iterations, &mut operation) < target && iterations < 1_048_576 {
            iterations *= 2;
        }
    }
    let mut times: Vec<_> = (0..samples)
        .map(|_| batch(iterations, &mut operation) / iterations)
        .collect();
    times.sort_unstable();
    Measurement {
        iterations,
        min: times[0],
        median: times[times.len() / 2],
        p90: times[(times.len() * 9).div_ceil(10) - 1],
        max: times[times.len() - 1],
    }
}
