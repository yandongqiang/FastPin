use super::*;

fn sparse_scroll(axis: StitchAxis, scroll: u32, format: PixelFormat) -> Frame {
    let (width, height) = match axis {
        StitchAxis::Vertical => (640, 384),
        StitchAxis::Horizontal => (384, 640),
    };
    let mut pixels = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let (cross, along) = match axis {
                StitchAxis::Vertical => (x, y),
                StitchAxis::Horizontal => (y, x),
            };
            let fixed = !(72..312).contains(&along) || cross < 512;
            // A repeating stationary sidebar favors 74px, while content moves 73px.
            let source = if fixed { along % 74 } else { along + scroll };
            let mut hash = cross.wrapping_mul(0xc2b2_ae35) ^ source.wrapping_mul(0x27d4_eb2d);
            hash ^= hash >> 16;
            hash = hash.wrapping_mul(0x7feb_352d);
            hash ^= hash >> 15;
            let value = if fixed || (544..624).contains(&cross) {
                (hash >> 24) as u8
            } else {
                255
            };
            match format {
                PixelFormat::Gray8 => pixels.push(value),
                PixelFormat::Rgb8 => pixels.extend_from_slice(&[value; 3]),
                PixelFormat::Rgba8 => pixels.extend_from_slice(&[value, value, value, 255]),
            }
        }
    }
    Frame::new(width, height, format, pixels).unwrap()
}

fn motion_observations(frame: &Frame, axis: StitchAxis, offset: i32) -> Vec<MatchObservation> {
    let layout = TileLayout::new(frame.width(), frame.height(), 32);
    let mut observations = Vec::new();
    for along in (88..296).step_by(16) {
        if !(80..304).contains(&(along - offset)) {
            continue;
        }
        for cross in (552..616).step_by(16) {
            let (x, y, dx, dy) = match axis {
                StitchAxis::Vertical => (cross, along, 0, offset),
                StitchAxis::Horizontal => (along, cross, offset, 0),
            };
            observations.push(MatchObservation {
                distance: 0.0,
                reference_x: (x - dx) as f32,
                reference_y: (y - dy) as f32,
                incoming_x: x as f32,
                incoming_y: y as f32,
                reference_tile: layout.index((x - dx) as f32, (y - dy) as f32).unwrap(),
                dx,
                dy,
            });
        }
    }
    observations
}

#[test]
fn refinement_preserves_and_corrects_motion_among_textured_fixed_ui() {
    for axis in [StitchAxis::Vertical, StitchAxis::Horizontal] {
        for format in [PixelFormat::Gray8, PixelFormat::Rgb8, PixelFormat::Rgba8] {
            let first = sparse_scroll(axis, 0, format);
            let second = sparse_scroll(axis, 73, format);
            for reverse in [false, true] {
                let (reference, incoming, expected) = if reverse {
                    (&second, &first, 73)
                } else {
                    (&first, &second, -73)
                };
                let observations = motion_observations(incoming, axis, expected);
                for center in [expected, expected - 1, expected + 1] {
                    let actual =
                        refine_motion_offset(reference, incoming, axis, center, 200, &observations);
                    assert_eq!(
                        actual.0, expected,
                        "{axis:?} {format:?} reverse={reverse} center={center}: {actual:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn refinement_requires_distributed_texture_and_respects_motion_limits() {
    for axis in [StitchAxis::Vertical, StitchAxis::Horizontal] {
        let reference = sparse_scroll(axis, 0, PixelFormat::Gray8);
        let incoming = sparse_scroll(axis, 73, PixelFormat::Gray8);
        let observations = motion_observations(&incoming, axis, -73);
        assert_eq!(
            refine_motion_offset(&reference, &incoming, axis, -72, 200, &[]),
            (-72, 1.0)
        );
        let concentrated = vec![observations[0]; 100];
        assert_eq!(
            refine_motion_offset(&reference, &incoming, axis, -72, 200, &concentrated),
            (-72, 1.0)
        );
        let blank = Frame::new(
            incoming.width(),
            incoming.height(),
            PixelFormat::Gray8,
            vec![128; incoming.pixels().len()],
        )
        .unwrap();
        assert_eq!(
            refine_motion_offset(&blank, &blank, axis, -72, 200, &observations),
            (-72, 1.0)
        );
        // Texture constant along the scroll axis gives every neighbor the same
        // supported score. Refinement must retain the coarse mode on that tie.
        let pixels = (0..incoming.height())
            .flat_map(|y| {
                (0..incoming.width()).map(move |x| {
                    let cross = match axis {
                        StitchAxis::Vertical => x,
                        StitchAxis::Horizontal => y,
                    };
                    (cross * 17) as u8
                })
            })
            .collect();
        let repeated = Frame::new(
            incoming.width(),
            incoming.height(),
            PixelFormat::Gray8,
            pixels,
        )
        .unwrap();
        assert_eq!(
            refine_motion_offset(&repeated, &repeated, axis, -72, 200, &observations),
            (-72, 0.0)
        );
        let limited = refine_motion_offset(&reference, &incoming, axis, -72, 72, &observations);
        assert!(limited.0.abs() <= 72);
        let edge = Frame::new(5, 7, PixelFormat::Gray8, vec![128; 35]).unwrap();
        assert_eq!(
            refine_motion_offset(&edge, &edge, axis, -2, 2, &observations),
            (-2, 1.0)
        );
    }
}

#[test]
fn refinement_balances_tiles_and_tolerates_pixel_noise() {
    for axis in [StitchAxis::Vertical, StitchAxis::Horizontal] {
        let reference = sparse_scroll(axis, 0, PixelFormat::Gray8);
        let mut incoming = sparse_scroll(axis, 73, PixelFormat::Gray8);
        let mut observations = motion_observations(&incoming, axis, -73);
        for pixel in incoming.pixels_mut().iter_mut() {
            *pixel = pixel.saturating_add(1);
        }
        // Corrupt one patch and duplicate its feature evidence. It must not
        // outweigh the independently supported motion in the remaining tiles.
        let bad = observations[0];
        let width = incoming.width();
        for y in bad.incoming_y as u32 - 6..=bad.incoming_y as u32 + 6 {
            for x in bad.incoming_x as u32 - 6..=bad.incoming_x as u32 + 6 {
                let pixel = &mut incoming.pixels_mut()[(y * width + x) as usize];
                *pixel = 255 - *pixel;
            }
        }
        observations.extend(std::iter::repeat_n(bad, 500));
        // Stationary descriptors from the repeating sidebar are not motion support.
        for observation in observations.clone() {
            observations.push(MatchObservation {
                dx: 0,
                dy: 0,
                ..observation
            });
        }
        let actual = refine_motion_offset(&reference, &incoming, axis, -74, 200, &observations);
        assert_eq!(actual.0, -73, "{axis:?}: {actual:?}");
        assert!(actual.1 > 0.0 && actual.1 < 0.02);
    }
}

fn document(axis: StitchAxis, scroll: u32, extent: u32) -> Frame {
    let (width, height) = match axis {
        StitchAxis::Vertical => (320, extent),
        StitchAxis::Horizontal => (extent, 320),
    };
    let pixels = (0..height)
        .flat_map(|y| {
            (0..width).map(move |x| {
                let (cross, along) = match axis {
                    StitchAxis::Vertical => (x, y + scroll),
                    StitchAxis::Horizontal => (y, x + scroll),
                };
                let mut hash = cross.wrapping_mul(0xc2b2_ae35) ^ along.wrapping_mul(0x27d4_eb2d);
                hash ^= hash >> 16;
                hash = hash.wrapping_mul(0x7feb_352d);
                hash ^= hash >> 15;
                (hash >> 24) as u8
            })
        })
        .collect();
    Frame::new(width, height, PixelFormat::Gray8, pixels).unwrap()
}

#[test]
fn refinement_keeps_reference_coordinates_after_rejection_and_reverse_scroll() {
    for axis in [StitchAxis::Vertical, StitchAxis::Horizontal] {
        let options = crate::StitchOptions {
            axis,
            record_decisions: true,
            ..Default::default()
        };
        let first = document(axis, 0, 256);
        let mut stitcher = crate::Stitcher::new(options).unwrap();
        stitcher.push(first.clone()).unwrap();
        let blank = Frame::new(
            first.width(),
            first.height(),
            PixelFormat::Gray8,
            vec![128; first.pixels().len()],
        )
        .unwrap();
        assert_eq!(stitcher.push(blank).unwrap().unwrap().accepted_offset, None);
        let recovered = stitcher.push(document(axis, 40, 256)).unwrap().unwrap();
        assert_eq!(
            recovered.accepted_offset,
            Some(-40),
            "{axis:?}: {recovered:?}"
        );
        assert_eq!(recovered.after.viewport_position, 40);
        for (scroll, offset) in [(80, -40), (40, 40), (0, 40)] {
            let decision = stitcher.push(document(axis, scroll, 256)).unwrap().unwrap();
            assert_eq!(
                decision.accepted_offset,
                Some(offset),
                "{axis:?}: {decision:?}"
            );
            assert_eq!(decision.after.viewport_position, i64::from(scroll));
        }
        assert_eq!(stitcher.finish().unwrap().image, document(axis, 0, 336));
    }
}
