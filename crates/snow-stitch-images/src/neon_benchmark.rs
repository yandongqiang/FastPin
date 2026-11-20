//! Narrow paired measurements; callers must run one Release benchmark at a time.
use std::{fmt::Debug, hint::black_box, time::Instant};

pub(crate) fn paired<T: PartialEq + Debug>(
    name: &str,
    iterations: u32,
    mut scalar: impl FnMut() -> T,
    mut neon: impl FnMut() -> T,
) {
    if cfg!(debug_assertions) {
        panic!("use the ARM64 performance preset's Release profile");
    }
    assert_eq!(scalar(), neon(), "benchmark inputs must preserve results");
    let mut samples = [Vec::new(), Vec::new()];
    for round in 0..9 {
        for backend in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
            let started = Instant::now();
            for _ in 0..iterations {
                if backend == 0 {
                    black_box(scalar());
                } else {
                    black_box(neon());
                }
            }
            if round > 0 {
                samples[backend].push(started.elapsed().as_nanos() / u128::from(iterations));
            }
        }
    }
    for sample in &mut samples {
        sample.sort_unstable();
    }
    let scalar_ns = samples[0][samples[0].len() / 2];
    let neon_ns = samples[1][samples[1].len() / 2];
    println!(
        "neon_benchmark name={name} scalar_median_ns={scalar_ns} neon_median_ns={neon_ns} speedup={:.3}",
        scalar_ns as f64 / neon_ns.max(1) as f64
    );
}
