//! Release corpus comparison through the production incremental stitcher.
//! Usage: corpus_benchmark INPUT OUTPUT [SAMPLES] [BASELINE_OUTPUT]
//! Decoding, pixel verification, and output encoding are outside timed stitching.

use std::{
    collections::HashSet,
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use snow_stitch_images::{Frame, StitchDecision, StitchOptions, Stitcher, perf};

#[derive(Debug, PartialEq, Eq)]
struct CorpusGroup {
    name: String,
    paths: Vec<PathBuf>,
}

fn discover_corpus(
    input: &Path,
    output: &Path,
    baseline: Option<&Path>,
) -> Result<Vec<CorpusGroup>, Box<dyn Error>> {
    // Resolve roles once so relative paths, dot components and directory links
    // cannot turn a generated output directory into an input sequence.
    let input = fs::canonicalize(input)?;
    let output = fs::canonicalize(output)?;
    let baseline = baseline.map(fs::canonicalize).transpose()?;
    if input == output || baseline.as_ref() == Some(&input) {
        return Err("input directory must differ from output and baseline directories".into());
    }
    if baseline.as_ref() == Some(&output) {
        return Err("output directory must differ from baseline directory".into());
    }

    let mut directories: Vec<_> = fs::read_dir(&input)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()?;
    directories.retain(|path| path.is_dir());
    directories.sort();
    // Root PNGs and child sequences are independent. Empty or unrelated child
    // directories must not hide frames stored directly beneath INPUT.
    directories.insert(0, input);
    let mut groups = Vec::new();
    let mut output_names = HashSet::new();
    for directory in directories {
        let resolved = fs::canonicalize(&directory)?;
        if resolved == output || baseline.as_ref() == Some(&resolved) {
            continue;
        }
        let mut paths: Vec<_> = fs::read_dir(&directory)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<_, _>>()?;
        paths.retain(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        });
        paths.sort();
        if paths.is_empty() {
            continue;
        }
        let name = directory
            .file_name()
            .ok_or("image group has no name")?
            .to_string_lossy()
            .into_owned();
        // The root and a child can have the same basename. Reject ambiguous
        // report identities and filenames before any benchmark artifacts exist.
        #[cfg(any(windows, target_os = "macos"))]
        let output_name = name.to_lowercase();
        #[cfg(not(any(windows, target_os = "macos")))]
        let output_name = name.clone();
        if !output_names.insert(output_name) {
            return Err(format!(
                "image group name {name:?} would overwrite another group's output"
            )
            .into());
        }
        groups.push(CorpusGroup { name, paths });
    }
    if groups.is_empty() {
        return Err("input contains no PNG image groups".into());
    }
    Ok(groups)
}

fn main() -> Result<(), Box<dyn Error>> {
    if cfg!(debug_assertions) {
        return Err("corpus benchmark requires the performance Release profile".into());
    }
    let mut arguments = env::args_os().skip(1);
    let input = PathBuf::from(arguments.next().ok_or("missing input directory")?);
    let output = PathBuf::from(arguments.next().ok_or("missing output directory")?);
    let samples: usize = arguments
        .next()
        .map(|value| value.to_string_lossy().parse())
        .transpose()?
        .unwrap_or(3);
    if samples == 0 {
        return Err("sample count must be positive".into());
    }
    let baseline = arguments.next().map(PathBuf::from);
    if arguments.next().is_some() {
        return Err("too many arguments".into());
    }
    fs::create_dir_all(&output)?;
    let corpus = discover_corpus(&input, &output, baseline.as_deref())?;
    let baseline_report: Option<serde_json::Value> = baseline
        .as_ref()
        .map(|directory| {
            let bytes = fs::read(directory.join("results.json"))?;
            Ok::<_, Box<dyn Error>>(serde_json::from_slice(&bytes)?)
        })
        .transpose()?;
    let mut groups = Vec::new();
    for CorpusGroup { name, paths } in corpus {
        let decode_started = Instant::now();
        let frames: Vec<_> = paths
            .iter()
            .map(|path| {
                let frame = Frame::decode(path)?;
                let geometry = frame.geometry();
                Frame::from_shared_buffer(
                    geometry.width,
                    geometry.height,
                    geometry.pixel_format,
                    Arc::new(frame.into_buffer()),
                )
            })
            .collect::<Result<_, _>>()?;
        let decode_ns = decode_started.elapsed().as_nanos() as u64;
        let expected = baseline_report.as_ref().and_then(|report| {
            report["groups"]
                .as_array()?
                .iter()
                .find(|group| group["name"].as_str() == Some(&name))
        });
        if baseline_report.is_some() && expected.is_none() {
            return Err(format!("baseline has no image group {name}").into());
        }
        let expected_decisions: Option<Vec<StitchDecision>> = expected
            .map(|group| serde_json::from_value(group["decisions"].clone()))
            .transpose()?;
        let mut timings = Vec::new();
        let mut decisions = None;
        let mut geometry = None;
        let mut checksum = 0;
        for iteration in 0..=samples {
            let mut stitcher = Stitcher::new(StitchOptions {
                record_decisions: true,
                ..StitchOptions::default()
            })?;
            perf::reset();
            let started = Instant::now();
            for frame in &frames {
                stitcher.push(frame.clone())?;
            }
            let push_ns = started.elapsed().as_nanos() as u64;
            let recorded = perf::snapshot();
            let started = Instant::now();
            let result = stitcher.finish()?;
            let finish_ns = started.elapsed().as_nanos() as u64;
            let actual_decisions = serde_json::to_value(&result.decisions)?;
            if let Some(expected) = &expected_decisions {
                if result.decisions != *expected {
                    fs::write(
                        output.join(format!("{name}-different-decisions.json")),
                        serde_json::to_vec_pretty(&actual_decisions)?,
                    )?;
                    result.image.encode(output.join(format!("{name}.png")))?;
                    return Err(format!("motion decisions differ from baseline for {name}").into());
                }
                if iteration == 1 {
                    let pixels =
                        Frame::decode(baseline.as_ref().unwrap().join(format!("{name}.png")))?;
                    if result.image != pixels {
                        return Err(
                            format!("stitched pixels differ from baseline for {name}").into()
                        );
                    }
                }
            }
            if iteration == 0 {
                continue;
            }
            timings.push(serde_json::json!({
                "push_ns": push_ns,
                "finish_ns": finish_ns,
                "stage_ns": recorded.elapsed_ns,
                "stage_calls": recorded.calls,
            }));
            if iteration == 1 {
                checksum = result
                    .image
                    .pixels()
                    .iter()
                    .fold(0xcbf2_9ce4_8422_2325_u64, |hash, value| {
                        (hash ^ u64::from(*value)).wrapping_mul(0x100_0000_01b3)
                    });
                geometry = Some(result.image.geometry());
                result.image.encode(output.join(format!("{name}.png")))?;
                decisions = Some(actual_decisions);
            }
        }
        println!(
            "{name}: {} frames, {} measured runs, exact baseline comparison {}",
            frames.len(),
            samples,
            baseline.is_some()
        );
        groups.push(serde_json::json!({
            "name": name,
            "frame_count": frames.len(),
            "input_geometry": frames[0].geometry(),
            "output_geometry": geometry,
            "decode_ns": decode_ns,
            "pixel_checksum": checksum,
            "decisions": decisions,
            "samples": timings,
            "exact_baseline_match": baseline.is_some(),
        }));
        fs::write(
            output.join("results.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "input": input,
                "samples": samples,
                "warmups": 1,
                "timed_input": "immutable shared frame clones; decoding and encoding excluded",
                "groups": groups,
            }))?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(parent: &Path, name: &str) -> PathBuf {
        let path = parent.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn png(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, b"discovery does not decode image bytes").unwrap();
        fs::canonicalize(path).unwrap()
    }

    #[test]
    fn flat_input_retains_root_pngs_with_empty_nested_output() {
        let fixture = tempfile::tempdir().unwrap();
        let input = directory(fixture.path(), "input");
        let second = png(&input, "002.PNG");
        let first = png(&input, "001.png");
        let output = directory(&input, "output");
        directory(&input, "notes");
        assert_eq!(
            discover_corpus(&input, &output, None).unwrap(),
            vec![CorpusGroup {
                name: "input".to_owned(),
                paths: vec![first, second],
            }]
        );
    }

    #[test]
    fn grouped_input_excludes_existing_output_and_baseline_using_path_aliases() {
        let fixture = tempfile::tempdir().unwrap();
        let input = directory(fixture.path(), "input");
        let output = directory(&input, "output");
        let baseline = directory(&input, "baseline");
        png(&output, "generated.png");
        png(&baseline, "generated.png");
        let scroll = directory(&input, "scroll");
        let frame = png(&scroll, "001.png");
        assert_eq!(
            discover_corpus(
                &input.join("."),
                &output.join("..").join("output"),
                Some(&baseline.join(".")),
            )
            .unwrap(),
            vec![CorpusGroup {
                name: "scroll".to_owned(),
                paths: vec![frame],
            }]
        );
    }

    #[test]
    fn root_and_direct_child_sequences_are_discovered_independently() {
        let fixture = tempfile::tempdir().unwrap();
        let input = directory(fixture.path(), "input");
        let output = directory(fixture.path(), "output");
        let root = png(&input, "001.png");
        let z = png(&directory(&input, "z-scroll"), "001.png");
        let a = png(&directory(&input, "a-scroll"), "001.png");
        directory(&input, "ignored.png");
        png(&directory(&input, "notes/deeper"), "001.png");
        fs::write(input.join("readme.txt"), b"not a frame").unwrap();
        assert_eq!(
            discover_corpus(&input, &output, None).unwrap(),
            vec![
                CorpusGroup {
                    name: "input".to_owned(),
                    paths: vec![root],
                },
                CorpusGroup {
                    name: "a-scroll".to_owned(),
                    paths: vec![a],
                },
                CorpusGroup {
                    name: "z-scroll".to_owned(),
                    paths: vec![z],
                },
            ]
        );
    }

    #[test]
    fn flat_discovery_is_stable_after_nested_outputs_are_populated() {
        let fixture = tempfile::tempdir().unwrap();
        let input = directory(fixture.path(), "input");
        let frame = png(&input, "001.png");
        let output = directory(&input, "output");
        let baseline = directory(&input, "baseline");
        let before = discover_corpus(&input, &output, Some(&baseline)).unwrap();
        let generated = png(&output, "input.png");
        let baseline_png = png(&baseline, "input.png");
        let report = baseline.join("results.json");
        fs::write(&report, b"baseline report must stay unchanged").unwrap();
        assert_eq!(
            discover_corpus(&input, &output, Some(&baseline)).unwrap(),
            before
        );
        assert_eq!(
            before,
            vec![CorpusGroup {
                name: "input".to_owned(),
                paths: vec![frame],
            }]
        );
        assert_eq!(
            fs::read(generated).unwrap(),
            fs::read(baseline_png).unwrap()
        );
        assert_eq!(
            fs::read(report).unwrap(),
            b"baseline report must stay unchanged"
        );
    }

    #[test]
    fn output_nested_inside_a_sequence_does_not_exclude_its_source_frames() {
        let fixture = tempfile::tempdir().unwrap();
        let input = directory(fixture.path(), "input");
        let scroll = directory(&input, "scroll");
        let frame = png(&scroll, "001.png");
        let output = directory(&scroll, "output");
        png(&output, "generated.png");
        assert_eq!(
            discover_corpus(&input, &output, None).unwrap(),
            vec![CorpusGroup {
                name: "scroll".to_owned(),
                paths: vec![frame],
            }]
        );
    }

    #[test]
    fn overlapping_directory_roles_are_rejected_without_changing_files() {
        let fixture = tempfile::tempdir().unwrap();
        let input = directory(fixture.path(), "input");
        let output = directory(fixture.path(), "output");
        let frame = png(&input, "001.png");
        let generated = png(&output, "input.png");
        for (destination, baseline, expected) in [
            (
                input.join("."),
                None,
                "input directory must differ from output and baseline directories",
            ),
            (
                output.clone(),
                Some(input.join(".")),
                "input directory must differ from output and baseline directories",
            ),
            (
                output.clone(),
                Some(output.join(".")),
                "output directory must differ from baseline directory",
            ),
        ] {
            assert_eq!(
                discover_corpus(&input, &destination, baseline.as_deref())
                    .unwrap_err()
                    .to_string(),
                expected
            );
        }
        assert_eq!(fs::read(frame).unwrap(), fs::read(generated).unwrap());
        assert!(!output.join("results.json").exists());
    }

    #[test]
    fn duplicate_root_and_child_group_names_are_rejected_before_output() {
        let fixture = tempfile::tempdir().unwrap();
        let input = directory(fixture.path(), "input");
        let output = directory(fixture.path(), "output");
        png(&input, "001.png");
        png(&directory(&input, "input"), "001.png");
        assert_eq!(
            discover_corpus(&input, &output, None)
                .unwrap_err()
                .to_string(),
            "image group name \"input\" would overwrite another group's output"
        );
        assert_eq!(fs::read_dir(output).unwrap().count(), 0);
    }

    #[test]
    fn output_pngs_directories_and_deep_frames_do_not_make_an_empty_corpus_valid() {
        let fixture = tempfile::tempdir().unwrap();
        let input = directory(fixture.path(), "input");
        let output = directory(&input, "output");
        let baseline = directory(&input, "baseline");
        png(&output, "generated.png");
        png(&baseline, "generated.png");
        directory(&input, "not-a-file.png");
        png(&directory(&input, "notes/deeper"), "001.png");
        assert_eq!(
            discover_corpus(&input, &output, Some(&baseline))
                .unwrap_err()
                .to_string(),
            "input contains no PNG image groups"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_case_aliases_do_not_bypass_directory_exclusions() {
        let fixture = tempfile::tempdir().unwrap();
        let input = directory(fixture.path(), "Input");
        let output = directory(&input, "Output");
        let baseline = directory(&input, "Baseline");
        let frame = png(&input, "001.png");
        png(&output, "generated.png");
        png(&baseline, "generated.png");
        assert_eq!(
            discover_corpus(
                &PathBuf::from(input.to_str().unwrap().to_lowercase()),
                &PathBuf::from(output.to_str().unwrap().to_uppercase()),
                Some(&PathBuf::from(baseline.to_str().unwrap().to_lowercase())),
            )
            .unwrap(),
            vec![CorpusGroup {
                name: "Input".to_owned(),
                paths: vec![frame],
            }]
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn group_names_that_differ_only_in_case_cannot_overwrite_output() {
        let fixture = tempfile::tempdir().unwrap();
        let input = directory(fixture.path(), "Input");
        let output = directory(fixture.path(), "output");
        png(&input, "001.png");
        png(&directory(&input, "input"), "001.png");
        assert!(
            discover_corpus(&input, &output, None)
                .unwrap_err()
                .to_string()
                .contains("would overwrite another group's output")
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_links_to_generated_outputs_are_excluded() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let input = directory(fixture.path(), "input");
        let output = directory(fixture.path(), "output");
        let baseline = directory(fixture.path(), "baseline");
        let frame = png(&input, "001.png");
        png(&output, "generated.png");
        png(&baseline, "generated.png");
        symlink(&output, input.join("output-link")).unwrap();
        symlink(&baseline, input.join("baseline-link")).unwrap();
        assert_eq!(
            discover_corpus(&input, &output, Some(&baseline)).unwrap(),
            vec![CorpusGroup {
                name: "input".to_owned(),
                paths: vec![frame],
            }]
        );
    }
}
