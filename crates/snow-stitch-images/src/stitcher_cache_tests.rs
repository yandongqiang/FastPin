use super::*;
use crate::{MotionStage, PixelFormat};
use std::sync::Arc;

fn texture(axis: StitchAxis, cross: u32, position: u32, format: PixelFormat, seed: u32) -> Frame {
    let (width, height) = match axis {
        StitchAxis::Vertical => (cross, 256),
        StitchAxis::Horizontal => (256, cross),
    };
    let mut pixels = Vec::with_capacity((width * height * format.channels()) as usize);
    for y in 0..height {
        for x in 0..width {
            let (cross, primary) = match axis {
                StitchAxis::Vertical => (x, y + position),
                StitchAxis::Horizontal => (y, x + position),
            };
            let mut hash = (cross / 4).wrapping_mul(0xc2b2_ae35)
                ^ (primary / 4).wrapping_mul(0x27d4_eb2d)
                ^ seed;
            hash ^= hash >> 16;
            hash = hash.wrapping_mul(0x7feb_352d);
            let gray = (hash >> 24) as u8;
            pixels.extend(std::iter::repeat_n(gray, format.channels() as usize));
        }
    }
    Frame::new(width, height, format, pixels).unwrap()
}

fn animated(frame: &Frame, value: u8) -> Frame {
    let mut changed = frame.clone();
    let channels = frame.pixel_format().channels() as usize;
    for y in 32..56 {
        for x in 32..56 {
            let byte = (y * frame.width() as usize + x) * channels;
            changed.pixels_mut()[byte..byte + channels.min(3)].fill(value);
        }
    }
    changed
}

fn uncached(frames: &[Frame], options: StitchOptions) -> StitchResult {
    let mut estimator = VerticalMotionEstimator::new_for_axis(
        frames[0].geometry(),
        options.axis,
        options.estimator,
    )
    .unwrap();
    stitch_owned_with_estimator(
        frames.iter().cloned().map(Ok),
        options,
        |reference, previous, incoming, _| estimator.estimate(reference, previous, incoming),
    )
    .unwrap()
}

#[test]
fn cached_stitching_preserves_decisions_and_pixels_across_reference_transitions() {
    for axis in [StitchAxis::Vertical, StitchAxis::Horizontal] {
        for cross in [320, 1024] {
            for format in [PixelFormat::Gray8, PixelFormat::Rgb8, PixelFormat::Rgba8] {
                let frame = |position| texture(axis, cross, position, format, 0);
                let initial = frame(128);
                let mut border = initial.clone();
                border.pixels_mut()[0] ^= 0xff;
                let mut frames = vec![
                    initial.clone(),
                    animated(&initial, 0),
                    animated(&initial, 255),
                    animated(&initial, 255),
                    border,
                    initial.clone(),
                    frame(64),
                    frame(0),
                    initial.clone(),
                    animated(&initial, 0),
                    animated(&initial, 255),
                    frame(256),
                    frame(128),
                    frame(256),
                ];
                // Mixed shared and owned inputs must obey the same cache lifecycle.
                for index in (0..frames.len()).step_by(2) {
                    let source = &frames[index];
                    frames[index] = Frame::from_shared_buffer(
                        source.width(),
                        source.height(),
                        format,
                        Arc::new(source.pixels().to_vec()),
                    )
                    .unwrap();
                }
                let options = StitchOptions {
                    axis,
                    record_decisions: true,
                    ..Default::default()
                };
                let expected = uncached(&frames, options);
                let actual = stitch(&frames, options).unwrap();
                assert_eq!(actual, expected, "{axis:?} {cross} {format:?}");
                for branch in [
                    StitchBranch::Skip,
                    StitchBranch::NoMovement,
                    StitchBranch::Append,
                    StitchBranch::Prepend,
                    StitchBranch::Contained,
                ] {
                    assert!(
                        actual.decisions.iter().any(|d| d.branch == branch),
                        "missing {branch:?}"
                    );
                }
                assert!(actual.decisions.iter().any(|d| {
                    d.motion_diagnostics
                        .as_ref()
                        .is_some_and(|m| m.stage == MotionStage::IdenticalInterior)
                }));
                let mut streaming = Stitcher::new(options).unwrap();
                for (index, input) in frames.iter().enumerate() {
                    if index == 3 {
                        let wrong = Frame::new(5, 5, PixelFormat::Gray8, vec![0; 25]).unwrap();
                        assert!(streaming.push(wrong).is_err());
                    }
                    streaming.push(input.clone()).unwrap();
                }
                assert_eq!(streaming.finish().unwrap(), expected);
            }
        }
    }
}

#[test]
fn cached_scene_cut_streak_and_reset_match_uncached_estimation() {
    for axis in [StitchAxis::Vertical, StitchAxis::Horizontal] {
        let frames: Vec<_> = (0..5)
            .map(|seed| texture(axis, 1024, 0, PixelFormat::Rgba8, seed * 0x1234_5678))
            .collect();
        let options = StitchOptions {
            axis,
            record_decisions: true,
            ..Default::default()
        };
        let expected = uncached(&frames, options);
        assert!(expected.decisions.iter().any(|d| {
            d.motion_diagnostics
                .as_ref()
                .is_some_and(|m| m.stage == MotionStage::SceneCut)
        }));
        let mut streaming = Stitcher::new(options).unwrap();
        for frame in &frames {
            streaming.push(frame.clone()).unwrap();
        }
        assert_eq!(streaming.decisions(), expected.decisions);
        streaming.reset();
        // A new reference of the same geometry must not reuse the old scene.
        let new_frames: Vec<_> = frames.into_iter().rev().collect();
        let expected = uncached(&new_frames, options);
        for frame in &new_frames {
            streaming.push(frame.clone()).unwrap();
        }
        assert_eq!(streaming.finish().unwrap(), expected);
    }
}
