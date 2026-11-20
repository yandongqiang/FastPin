use super::*;
use std::{hint::black_box, time::Instant};

fn benchmark_frame(rgb: &Frame, format: PixelFormat, axis: StitchAxis) -> Frame {
    let (width, height) = match axis {
        StitchAxis::Vertical => (rgb.width(), rgb.height()),
        StitchAxis::Horizontal => (rgb.height(), rgb.width()),
    };
    let mut pixels = Vec::with_capacity((width * height * format.channels()) as usize);
    for y in 0..height {
        for x in 0..width {
            let (source_x, source_y) = match axis {
                StitchAxis::Vertical => (x, y),
                StitchAxis::Horizontal => (y, x),
            };
            let start = ((source_y * rgb.width() + source_x) * 3) as usize;
            match format {
                PixelFormat::Gray8 => pixels.push(luminance_at(rgb, source_x, source_y)),
                PixelFormat::Rgb8 => pixels.extend_from_slice(&rgb.pixels()[start..start + 3]),
                PixelFormat::Rgba8 => {
                    pixels.extend_from_slice(&rgb.pixels()[start..start + 3]);
                    pixels.push(255);
                }
            }
        }
    }
    Frame::new(width, height, format, pixels).unwrap()
}

fn timing_summary(samples: &[u64]) -> serde_json::Value {
    let mut samples = samples.to_vec();
    samples.sort_unstable();
    serde_json::json!({
        "count": samples.len(),
        "p50_ns": samples[(samples.len() * 50).div_ceil(100) - 1],
        "p95_ns": samples[(samples.len() * 95).div_ceil(100) - 1],
        "mean_ns": samples.iter().sum::<u64>() / samples.len() as u64,
    })
}

#[test]
#[ignore = "Release-only supported refinement benchmark using the local scrolling image fixture"]
fn refinement_release_benchmark() {
    if cfg!(debug_assertions) {
        panic!("use windows-msvc-performance Release");
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let input = std::env::var_os("SNOW_STITCH_REFINEMENT_BENCHMARK_IMAGE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join("snow_shot/test-imgs/scrollscreenshot-test.png"));
    let source = Frame::decode(&input).unwrap();
    let mut cases = Vec::new();
    for axis in [StitchAxis::Vertical, StitchAxis::Horizontal] {
        for format in [PixelFormat::Gray8, PixelFormat::Rgb8, PixelFormat::Rgba8] {
            for (top, crop_width, crop_height) in [
                (0, 320, 240),
                (8_000, 800, 600),
                (0, 1280, 720),
                (8_000, 1920, 1080),
                (0, source.width(), 1600),
                (8_000, source.width(), 1600),
                (18_000, source.width(), 1600),
            ] {
                let left = (source.width() - crop_width) / 2;
                let first = benchmark_frame(
                    &source.crop(left, top, crop_width, crop_height).unwrap(),
                    format,
                    axis,
                );
                let second = benchmark_frame(
                    &source
                        .crop(left, top + 75, crop_width, crop_height)
                        .unwrap(),
                    format,
                    axis,
                );
                let estimator = VerticalMotionEstimator::new_for_axis(
                    first.geometry(),
                    axis,
                    MotionEstimatorOptions::default(),
                )
                .unwrap();
                for reverse in [false, true] {
                    let (reference, incoming, strongest) = if reverse {
                        (&second, &first, 75)
                    } else {
                        (&first, &second, -75)
                    };
                    let (evidence, _) = pure_rust_feature_evidence(
                        reference,
                        incoming,
                        estimator.layout,
                        2500,
                        axis,
                        true,
                        &estimator.pyramid_plan,
                        Some(&estimator.sampling),
                        &mut None,
                    );
                    let expected = refine_motion_offset(
                        reference,
                        incoming,
                        axis,
                        strongest,
                        1200,
                        &evidence.observations,
                    );
                    assert_eq!(expected.0, strongest);
                    let mut samples = Vec::new();
                    for round in 0..21 {
                        let started = Instant::now();
                        let actual = black_box(refine_motion_offset(
                            black_box(reference),
                            black_box(incoming),
                            axis,
                            strongest,
                            1200,
                            black_box(&evidence.observations),
                        ));
                        let elapsed = started.elapsed().as_nanos() as u64;
                        assert_eq!(actual, expected);
                        if round > 0 {
                            samples.push(elapsed);
                        }
                    }
                    let timing = timing_summary(&samples);
                    eprintln!(
                        "{axis:?} {format:?} {crop_width}x{crop_height} top={top} reverse={reverse}: {} ns",
                        timing["p50_ns"]
                    );
                    cases.push(serde_json::json!({
                        "source": input,
                        "axis": axis, "format": format, "top": top, "reverse": reverse,
                        "width": first.width(), "height": first.height(),
                        "error": expected.1, "matches": evidence.observations.len(),
                        "timing": timing, "samples_ns": samples,
                    }));
                }
            }
        }
    }
    let output = std::env::var_os("SNOW_SCROLLING_ALGORITHM_PERF_OUTPUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join("build/windows-msvc-performance/refinement-results.json"));
    std::fs::write(&output, serde_json::to_vec_pretty(&cases).unwrap()).unwrap();
    eprintln!("refinement report: {}", output.display());
}

fn assert_same_map(left: &SimilarityMap, right: &SimilarityMap) {
    assert_eq!(left.layout(), right.layout());
    for tile in 0..left.layout().len() {
        assert_eq!(left.at_tile(tile), right.at_tile(tile));
        assert_eq!(left.texture_at(tile), right.texture_at(tile));
    }
}

#[test]
#[ignore = "Release-only paired zero-candidate benchmark using the local scrolling image fixture"]
fn zero_candidate_release_benchmark() {
    if cfg!(debug_assertions) {
        panic!("use windows-msvc-performance Release");
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let source = Frame::decode(root.join("snow_shot/test-imgs/scrollscreenshot-test.png")).unwrap();
    let mut cases = Vec::new();
    for axis in [StitchAxis::Vertical, StitchAxis::Horizontal] {
        for (width, height) in [(320, 240), (800, 600), (3840, 1600)] {
            for top in [0, 8_000, 18_000] {
                let left = (source.width() - width) / 2;
                let reference = benchmark_frame(
                    &source.crop(left, top, width, height).unwrap(),
                    PixelFormat::Rgba8,
                    axis,
                );
                let incoming = benchmark_frame(
                    &source.crop(left, top + 75, width, height).unwrap(),
                    PixelFormat::Rgba8,
                    axis,
                );
                let layout = TileLayout::new(reference.width(), reference.height(), 32);
                let reference_gray = GrayImage::from_frame(&reference).unwrap();
                let incoming_gray = GrayImage::from_frame(&incoming).unwrap();
                let zero = SimilarityMap::between(&reference_gray, &incoming_gray, layout, axis, 0);
                let mut observations = Vec::new();
                for y in (1..reference.height()).step_by(32) {
                    for x in (1..reference.width()).step_by(32) {
                        observations.push(MatchObservation {
                            distance: 10.0,
                            reference_x: x as f32,
                            reference_y: y as f32,
                            incoming_x: x as f32,
                            incoming_y: y as f32,
                            reference_tile: layout.index(x as f32, y as f32).unwrap(),
                            dx: 0,
                            dy: 0,
                        });
                    }
                }
                let expected = score_candidate(0, axis, &observations, &zero, &zero, &zero);
                let mut samples = [Vec::new(), Vec::new()];
                for round in 0..21 {
                    for reuse in if round % 2 == 0 {
                        [false, true]
                    } else {
                        [true, false]
                    } {
                        let started = Instant::now();
                        let recomputed = if reuse {
                            None
                        } else {
                            Some(SimilarityMap::between(
                                black_box(&reference_gray),
                                black_box(&incoming_gray),
                                layout,
                                axis,
                                0,
                            ))
                        };
                        let map = recomputed.as_ref().unwrap_or(black_box(&zero));
                        let actual = black_box(score_candidate(
                            0,
                            axis,
                            black_box(&observations),
                            &zero,
                            &zero,
                            map,
                        ));
                        let elapsed = started.elapsed().as_nanos() as u64;
                        assert_eq!(actual, expected);
                        assert_same_map(map, &zero);
                        if round > 0 {
                            samples[usize::from(reuse)].push(elapsed);
                        }
                    }
                }
                let original = timing_summary(&samples[0]);
                let reused = timing_summary(&samples[1]);
                eprintln!(
                    "zero candidate {axis:?} {width}x{height} top={top}: original={} ns reused={} ns",
                    original["p50_ns"], reused["p50_ns"]
                );
                cases.push(serde_json::json!({
                    "axis": axis, "width": width, "height": height, "top": top,
                    "original": original, "reused": reused, "samples_ns": samples,
                }));
            }
        }
    }
    let output = std::env::var_os("SNOW_SCROLLING_ALGORITHM_PERF_OUTPUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join("build/windows-msvc-performance/zero-candidate-results.json"));
    std::fs::write(&output, serde_json::to_vec_pretty(&cases).unwrap()).unwrap();
    eprintln!("zero-candidate report: {}", output.display());
}
