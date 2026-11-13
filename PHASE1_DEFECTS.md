# FastPin Phase 1 — Prioritised Defect List

Read-only audit of `d:/code/FastPin` at HEAD `4b039b7` (2026-11-12).
No business code, test, CI file or asset was modified; no commit, no push.

Severity: **P0** = crash/data loss on the normal path, **P1** = high-impact
common path or core-feature failure, **P2** = conditional/moderate,
**P3** = robustness/polish.

**P0: none found.**

**Status legend:** `Fixed (uncommitted)` = implemented in the working tree at
HEAD `4b039b7` + uncommitted changes; nothing committed, nothing pushed.
`Open` = unchanged by Phase 1.

---

## P1 — fix before Phase 2

### P1-01 — Long-capture tick runs all CPU and GPU-sync work on the GPUI main thread

**Status:** Fixed (uncommitted).
**Working-tree files:** `crates/fastpin/src/longworker.rs` (new),
`crates/fastpin/src/overlay/mod.rs`, `crates/fastpin/src/lib.rs`.

- **Symptom:** While a scrolling capture runs, the overlay's buttons, marching
  ants and status can freeze for a tick; on the non-live fallback the tick can
  exceed the 150 ms poll interval. No dropped/late-tick counter exists, so it is
  invisible except in the `diag.log` `elapsed_ms` line.
- **Root cause:** `overlay::spawn_long_poll` only moves the timer to the
  background executor (`overlay/mod.rs:1380-1392`); `this.update` runs
  `long_tick` on the UI thread (`1401-1511`). Inside it:
  `capture_region_live` / `capture_region_bgra` (`1411-1435`),
  `normalize_bgra` (`1436`), `bgra_to_gray` (`1437`),
  `classify_scroll_diag` (`1448`), `Stitcher::append/prepend` (`1456-1460`),
  `format!` status (`1483-1487`), `log_diag` (`1466`, `1490`) and
  `cx.notify` (`1509`). The fallback capture (`app.rs:150-166`) starts a fresh
  WGC session, composite and tonemap per tick (`capture/mod.rs:64-155`).
- **Fix:** Split the tick. Keep the GPU readback on the UI thread (the shared
  D3D11 context is single-threaded), send `(gen, px, w, h, axis)` to one
  long-lived worker that owns `prev_gray` + `Stitcher`, and apply the returned
  verdict on the UI thread. Add `t_capture/t_gray/t_sad/t_stitch` spans and a
  late-tick counter.
- **Regression test:** A pure-Rust session harness that feeds N synthetic frames
  through the worker and asserts stitched height/segments; a latency assertion
  is not portable, so assert "no work between capture and verdict on the UI
  thread" by construction (worker owns the state) and log spans in `diag.log`.

### P1-02 — `Stitcher::prepend` copies the entire buffer on every upward tick

**Status:** Fixed (uncommitted).
**Working-tree files:** `crates/fastpin/src/longcapture.rs`.

- **Symptom:** Scrolling up repeatedly makes each 150 ms tick progressively
  slower; at the 16384 px cap with a 2000 px wide region each upward tick copies
  ~131 MB (an 8K-wide selection: ~503 MB), on the UI thread.
- **Root cause:** `longcapture.rs:769-793` allocates a new `Vec` of
  `self.pixels.len() + bytes`, copies the new rows, then copies every existing
  byte, and replaces `self.pixels`. Cost is O(total height) per tick, quadratic
  over a session. `append` (`736-759`) uses `extend_from_slice` and can also
  realloc-copy up to the whole buffer when capacity grows.
- **Fix:** Keep a head reserve (e.g. reserve `max_height` up front or grow by a
  factor and use `copy_within` + a head offset), or store `Vec<Segment>` and
  flatten once in `into_pixels`. Either keeps the export contiguous.
- **Regression test:** `stitcher_prepend_preserves_content_and_accounts_height`
  already checks correctness; add a test that prepends 100× and asserts the
  pixel buffer is not reallocated (pointer stability) or that total copied bytes
  stays O(total) rather than O(n²).

### P1-03 — No unified image memory budget, checked arithmetic or fallible allocation

**Status:** Fixed (uncommitted) — partial routing, see the note at the end of
this item.
**Working-tree files:** `crates/fastpin/src/image_budget.rs` (new),
`crates/fastpin/src/longcapture.rs`, `crates/fastpin/src/pin_snap.rs`,
`crates/fastpin/src/codec/mod.rs`, `crates/fastpin/src/pin.rs`,
`crates/fastpin/src/pin_edit.rs`, `crates/fastpin/src/overlay/mod.rs`,
`crates/fastpin/src/lib.rs`.

- **Symptom:** A large pin group, an 8K-wide long capture, or a 16k-tall pin can
  request hundreds of MB to ~1 GiB per buffer. With `panic = "abort"`
  (`Cargo.toml` profile) an allocation failure kills the process instead of
  showing an error.
- **Root cause:**
  - `pin_snap::compose` caps only each side at 16384 (`pin_snap.rs:262,317`)
    and allocates `vec![0u8; row * ch]` (`322`) — 16384×16384×4 B = 1 GiB.
  - `Stitcher::new/append/prepend` cap only height (`longcapture.rs:703-717,
    736-759, 769-793`); width is unbounded and `width*4*height` is unchecked.
  - `bgra_to_gray` (`19-25`), `rotate_bgra90/270` (`559-590`) use unchecked
    `width*height` / `width*height*4`.
  - `codec::set_clipboard_dib` does `u32` math (`codec/mod.rs:100-101`).
  - No `try_reserve` anywhere; no max pixels/bytes; no shared accounting.
- **Fix:** One `budget` module with `MAX_SIDE`, `MAX_PIXELS`, `MAX_BYTES`,
  `checked_bytes(w,h,bpp) -> Option<usize>`, `alloc_bgra -> Result<Vec<u8>>`
  (`try_reserve_exact`) and `check_canvas`. Route `Stitcher`, rotates,
  `compose`, `crop`, `render_image`, `pin_edit`, `set_clipboard_dib`,
  `ocr::to_rgb`, `load_watermark_image` and `export_*` through it. Surface
  "image too large" in the panel/status.
- **Regression test:** Unit tests at the limit and one byte over, for each
  public entry: `compose(16384×16384)` returns `Err` (not abort),
  `Stitcher::new(7680, 16384)` is accepted only if under `MAX_BYTES`, and
  `checked_bytes(usize::MAX, 2, 4)` returns `None`.

**Residue (still Open, tracked under P2-05/P2-09/P3-05):** `ocr::to_rgb`,
`composite::load_watermark_image`, `pin::render_image` and the annotation
backdrops are not routed through the budget yet.

### P1-04 — GPU device reset leaves captures, the freeze visual and live WGC sessions broken

**Status:** Fixed (uncommitted) — decision logic unit-tested; a real TDR is still
manual acceptance.
**Working-tree files:** `crates/fastpin/src/gpu_lifecycle.rs` (new),
`crates/fastpin/src/capture/d3d.rs`, `crates/fastpin/src/capture/mod.rs`,
`crates/fastpin/src/capture/wgc.rs`, `crates/fastpin/src/gpui_bridge.rs`,
`crates/fastpin/src/app.rs`, `crates/fastpin/src/overlay/mod.rs`,
`crates/fastpin/src/lib.rs`, `third_party/gpui-pre/src/app.rs`,
`third_party/gpui-pre/src/platform.rs`,
`third_party/gpui-pre-windows/src/platform.rs`,
`patches/gpui-pre-0.3.9-fastpin-overlay.patch`,
`patches/gpui-pre-windows-0.3.9-fastpin-overlay.patch`.

- **Symptom:** A TDR/driver reset during an overlay or a long capture leaves the
  frozen background gone (live desktop visible under the selection UI) and
  export/OCR/pin/magnifier failing for that session; a long capture keeps
  polling forever without progress.
- **Root cause:**
  - The app only hooks `on_gpu_reset_begin` and drops shaders + the freeze swap
    (`app.rs:95-101`, `d3d.rs:124-127`, `gpui_bridge.rs:22-24`). There is no
    reset-end hook (`third_party/gpui-pre/src/app.rs:1965` is the only one).
  - gpui-pre recreates `DirectComposition` on device loss and re-attaches only
    the UI swap chain (`third_party/gpui-pre-windows/src/directx_renderer.rs:
    309-334`); the new `freeze_visual` stays empty (`1043-1068`).
  - `Gpu` (`capture/d3d.rs:142-150`), `CapturedDesktop` (`capture/mod.rs:27-39`)
    and `LongLive` (`overlay/mod.rs:243-249`) carry no device generation;
    only `GpuShaders` does (`d3d.rs:72-92`).
  - `cap.sdr_display` is then read with the new context
    (`app.rs:137`, `overlay/mod.rs:1708,1731`), i.e. a cross-device copy.
  - `WgcLive` was created with the old device (`capture/wgc.rs:158-159`); every
    `take_latest` fails and `long_tick` swallows it (`overlay/mod.rs:1428`).
- **Fix:** Add a `generation: u64` to `Gpu`/`CapturedDesktop`/`LongLive` and
  validate it at every use. On reset begin, mark them invalid, cancel the long
  capture, clear the freeze visual and remember that a re-publish is needed; add
  a reset-end hook to the local gpui patch that calls back into the app to
  re-create/re-publish the freeze chain (or close the overlay). Restart or
  terminate the live WGC session on the new device.
- **Regression test:** A host-level test with a fake `Gpu` generation counter is
  not meaningful; cover the decision logic by extracting
  `is_current(gen)` checks and unit-testing them. Manual: TDR during overlay and
  during long capture (rows 9-12 of the acceptance matrix).

### P1-05 — Static bottom/top strip stalls long capture; no seam validation

**Status:** Fixed (uncommitted) — the stall is now reported with a reason; an
alternate strip position is still not tried.
**Working-tree files:** `crates/fastpin/src/longcapture.rs`,
`crates/fastpin/src/longworker.rs` (new), `crates/fastpin/src/overlay/mod.rs`.

- **Symptom:** On pages with a sticky footer (down) or sticky header (up), the
  status keeps saying "滚动页面以继续截图" but no rows are ever added. On
  partially repeated content the stitch can be accepted with a wrong offset and
  silently duplicate/drop rows.
- **Root cause:** The strip is the bottom 32 rows for down and the top 32 for up
  (`longcapture.rs:167-174`, `overlay/mod.rs:1441`). A static band occupies
  exactly that strip, so SAD = 1.0 and `new_rows = 0`, which is rejected
  (`classify_scroll_diag`, `451-482`). No alternate strip position is tried,
  and `frame_change_ratio` is only used for the "too fast" hint
  (`overlay/mod.rs:1504-1506`). There is no post-stitch seam check.
- **Fix:** When `score == 1.0 && new_rows == 0 && changed` is high, retry with
  the strip at 3/4 or 1/2 height; if still static, show a "fixed header/footer
  detected — shrink the selection" message. Add a `seam_sad` check (compare
  2-4 rows above the seam with the already-stitched tail) and log it with the
  score/ratio.
- **Regression test:** Synthetic sticky footer: 40 static rows at the bottom,
  content above scrolling by 8 px per frame; assert rows are still accepted (or
  a `stalled` report is produced). Add the §7.2 matrix cases.

---

## P2 — schedule after P1 (status: Open)

### P2-01 — `start_live_session` blocks the UI thread for up to 500 ms

- **Symptom:** Pressing the long-capture button can hang the overlay for up to
  half a second before the bar responds.
- **Root cause:** `overlay/mod.rs:1260` calls `start_live_session`
  (`1358-1378`) synchronously; `WgcLive::wait_first` busy-sleeps in 5 ms steps
  (`capture/wgc.rs:190-206`) up to `Duration::from_millis(500)`.
- **Fix:** Start the session on a worker and report readiness through the long
  state; show "starting" in the bar. Or poll with a short async timer instead
  of `thread::sleep`.
- **Regression test:** Extract the readiness predicate and test it with a fake
  channel; manual timing check on device start.

### P2-02 — Diagnostic logging is synchronous file I/O per tick

- **Symptom:** Every 150 ms the UI thread opens, writes and flushes
  `diag.log`; on a slow/AV-scanned profile this becomes a visible per-tick cost.
- **Root cause:** `long_tick` calls `perf::log_diag` on both branches
  (`overlay/mod.rs:1466,1490`); `log_diag` opens the file and flushes every call
  (`perf.rs:116-148`).
- **Fix:** Buffer lines in a channel and flush on a worker; or keep the file
  handle open once per session; gate verbose tick logging behind a config flag.
- **Regression test:** Not a unit test; measure with the span added in P1-01.

### P2-03 — Pin compose/export silently drops tiles and marks

- **Symptom:** A group export or an edit commit can be missing a tile or the
  marks on one tile, with only a log line.
- **Root cause:** `pin_snap.rs:325` skips a tile whose buffer is shorter than
  `w*h*4` (or zero-sized); `pin_edit.rs:226` uses `crop(..)?` inside a
  `filter_map`, dropping the tile when the rounded dest falls outside the
  canvas; `pin.rs:1373-1376` only logs "group too large to compose".
- **Fix:** Return `Result`/a list of skipped ids, fail the whole export when a
  tile is invalid, and show a user-visible error. Make `tile_dest`/`crop`
  agree on the canvas bounds (clamp the dest to the canvas).
- **Regression test:** Pass a deliberately short tile buffer to `compose` and
  assert `Err` (not a partial image); add a 1 px-rounding case to the
  `flatten` tests.

### P2-04 — Pin edit compositing runs on the UI thread and retains a full backdrop

- **Symptom:** Starting/finishing an edit on a large group freezes the UI; the
  backdrop (`bw*bh*4`) is retained for the whole edit and another canvas is
  allocated during flatten.
- **Root cause:** `pin_edit::start` calls `compose` synchronously
  (`pin_edit.rs:103-126`); `finish` calls `compose` + `apply_annotations` on the
  UI thread (`150-231`); `GroupEdit.backdrop` lives until the edit ends.
- **Fix:** Compose the backdrop on a worker at edit start (with a loading
  state) and run flatten off-thread, or reuse the already-composed group texture
  instead of a second canvas.
- **Regression test:** The existing `pin_edit::tests::flatten_*` tests already
  assert correctness; add a size guard test via P1-03.

### P2-05 — Every pin stores the image twice

- **Symptom:** A 4K pin costs ~33 MB for `pixels` + ~33 MB for the cloned
  `RgbaImage` + the GPU texture; a 16k-tall pin costs >500 MB.
- **Root cause:** `pin::render_image` clones the whole `Arc<Vec<u8>>` into
  `image::RgbaImage::from_raw` (`pin.rs:366-369`); `PinView` keeps both
  (`595-601`).
- **Fix:** Build `RenderImage` from a shared/copy-on-write buffer, or drop
  `pixels` while not composing/editing and rebuild it from the render image on
  demand. Route the size check through P1-03.
- **Regression test:** A size-accounting unit test (bytes held per pin) and a
  manual 4K/16k pin check.

### P2-06 — Translation response body has no size limit

- **Symptom:** A broken or hostile endpoint can make the app allocate an
  arbitrarily large payload (chunks up to 4 GiB) and exhaust memory.
- **Root cause:** `translate.rs:278-298` loops on
  `WinHttpQueryDataAvailable` and grows `payload` with no cap.
- **Fix:** Cap the total (e.g. 4-8 MiB) and fail with "response too large";
  optionally honor `Content-Length` when present.
- **Regression test:** Extract the read loop behind a small `Read`/iterator and
  feed it a fake stream that exceeds the cap; assert `Err`.

### P2-07 — OCR engine lock held across inference; one idle timer thread per call

- **Symptom:** A slow/hung inference blocks every later OCR/translate request
  with no cancellation; each recognition spawns a thread that sleeps 60 s.
- **Root cause:** `ocr/mod.rs:35-45` holds the `ENGINE` mutex across
  `predict`; `schedule_idle_drop` spawns a detached thread per call
  (`214-227`).
- **Fix:** Add a cancellation/epoch check and a single shared idle timer (or a
  background sweep); consider a per-request timeout if ORT supports it.
- **Regression test:** Test the "last_used" drop decision as a pure function
  (already partially covered by the `classify` tests); add a concurrency test
  that two calls do not both load the engine.

### P2-08 — Clipboard DIB size math is unchecked and doubles the export buffer

- **Symptom:** Extreme sizes can wrap `u32` arithmetic and copy out of bounds;
  a 16k image needs the pixel buffer plus a full DIB copy.
- **Root cause:** `codec/mod.rs:100-101` computes `packed_stride = width*4` and
  `image_size = packed_stride*height` in `u32`; `set_clipboard_dib` allocates
  `header + image` without a checked path (`103-130`).
- **Fix:** Use the P1-03 checked sizes; for very large images offer PNG on the
  clipboard (or refuse with a clear message) instead of a 1 GiB DIB.
- **Regression test:** A size-check unit test at `u32` boundaries; a manual
  paste test at 4K/16k.

### P2-09 — Watermark image decode has no size cap

- **Symptom:** A very large user watermark file spikes memory during export
  (decode + premultiplied copy).
- **Root cause:** `composite::load_watermark_image` calls `image::open` and
  `to_rgba8` with no dimension/pixel check (`overlay/composite.rs:1111-1131`).
- **Fix:** Check file size and dimensions before decode; downscale to the
  capture's size.
- **Regression test:** Feed a generated 20000×20000 PNG header/fixture and
  assert the loader refuses it.

### P2-10 — CI is red at HEAD and fmt is not gated

- **Symptom:** `ci.yml` fails on a clean checkout at both the clippy and test
  steps; formatting drift accumulates unseen.
- **Root cause:**
  - `.github/workflows/ci.yml:9` runs `clippy ... -D warnings`; the tree has 5
    warnings (`overlay/composite.rs:1034`, `config.rs:153`, `config.rs:479`,
    `config.rs:583`, `config.rs:607`).
  - `.github/workflows/ci.yml:10` runs the same test suite;
    `settings::tests::source_only_keeps_legacy_migration_literals` fails on
    `README.md:29`, because the exemption marker at `settings/mod.rs:1954` is
    `二次开发` while the line says `二次vibe coding开发`.
  - No `cargo fmt --check` step; `cargo fmt --check` currently reports 52 hunks
    in 16 files.
- **Fix:** Add the fmt check, clean the five lints, and decide the README/test
  exemption with the user (do not silently edit `README.md` or weaken the
  scan — the credit line is intentionally user-owned).
- **Regression test:** CI itself; locally run all four commands before pushing.

### P2-11 — Long-capture quality test matrix is incomplete

- **Symptom:** Sticky header/footer, repeated table rows, dynamic content, fast
  scroll, 1-2 px steps and seam continuity have no deterministic coverage.
- **Root cause:** `longcapture.rs` tests cover geometry/SAD/reconstruction
  (`838-1657`) but not these scenarios; `overlay` has no long-capture test at
  all.
- **Fix / tests:** Add the §7.2 matrix: sticky footer, period-16 repeated rows,
  moving block, fast scroll, three 1 px steps, mid-session content swap, seam
  SAD. Use synthetic frames and drive `classify_scroll_diag` + `Stitcher`
  end-to-end.
- **Regression test:** The new tests above.

### P2-12 — Long-capture capture errors are swallowed forever

- **Symptom:** If WGC/readback starts failing (device reset, size change,
  protected content), the session keeps polling with no error count and no user
  feedback.
- **Root cause:** `overlay/mod.rs:1427-1433` treats every non-matching size and
  every `Err` as "skip this frame, continue"; only the "too fast" case changes
  the status (`1504-1506`).
- **Fix:** Count consecutive failures; after N (e.g. 5) show
  "capture stopped — restart" and stop or re-create the live session.
- **Regression test:** Inject a failing capture closure and assert the state
  transitions to a stopped/error status after N ticks.

---

## P3 — polish / robustness (status: Open)

### P3-01 — Stitch rejection is reported as the height cap

- `Stitcher::append/prepend` return `false` for a width mismatch, a zero-row
  append or a budget hit; `long_tick` maps every `false` to
  `state.status = "已达最大高度"` (`overlay/mod.rs:1473-1477`). Distinguish the
  three cases. Test: unit test each rejection reason on `Stitcher`, then a
  state-level test for the status string.

### P3-02 — `set_clipboard_dib` uses unchecked `u32` size math

- `codec/mod.rs:100-101`; move to the budget's `checked_bytes`. Test at
  `u32::MAX` boundaries.

### P3-03 — `move_windows` leaks the deferred-window batch on error

- `pin.rs:492-504` returns without `EndDeferWindowPos` when `DeferWindowPos`
  fails. Call it in a guard/drop. Test: hard to unit test; code review fix.

### P3-04 — Group members are not clamped during resize/zoom

- `pin.rs:1104-1112` applies `zoom0 * factor` without a per-member clamp;
  `apply_zoom` (`1302-1321`) does not clamp either. Clamp each member and
  derive the common factor from the clamped values. Test: mixed-zoom group
  resize; assert every member stays within `[MIN_ZOOM, MAX_ZOOM]` and
  `MIN_RESIZE_SIDE`.

### P3-05 — OCR input allocation has no cap

- `ocr/mod.rs:229-246` allocates `w*h*3`; route through P1-03 and downscale
  before OCR when over the limit. Test: a 16k² input returns a budget error
  instead of allocating ~786 MB.

### P3-06 — Translation timeout multiplication can overflow

- `translate.rs:223`: `timeout_seconds * 1000` in `u32` before `as i32`.
  Clamp the config value (1..=120). Test: pure `timeout_ms` helper.

### P3-07 — Generation is checked only before the capture

- `overlay/mod.rs:1406` checks `state.gen != gen`; the state is re-borrowed
  after the capture (`1438`) without a second check. Safe today (single
  thread); becomes a stale-result bug the moment P1-01 moves work off-thread.
  Carry `gen` in the worker result and re-check before mutating. Test: feed a
  result tagged with an old generation and assert it is discarded.

### P3-08 — Pin buffer slicing can panic; ✓ can overlap on tiny pins

- `pin::repack_opaque` slices `pixels[start..start+tight]` without a length
  check (`pin.rs:1942-1954`) — a short crop panics. `stitch_button_origin`
  clamps to `(w-28).max(6)` (`1421-1428`), which overlaps the ✕ when the pin is
  narrower than 28 px. Validate the buffer and clamp the button inside the
  client rect. Test: `repack_opaque` with a short buffer returns `None`; a
  20 px pin keeps the two buttons disjoint.
