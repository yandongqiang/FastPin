# FastPin Phase 1 — Read-Only Audit Report

- **Repository:** `d:/code/FastPin` (Windows 11, Rust 1.99.1 / cargo 1.99.1)
- **HEAD:** `4b039b7` (`main`, 6 commits ahead of `origin/main`)
- **Working tree:** only `?? .npmcache/` plus the two report files added by this audit
- **Scope:** read-only; **no business code, build script, test, CI or asset was modified**, no commit, no push
- **Round:** Phase 1 audit, 2026-11-12

---

## 1. Executive summary

FastPin is a ~28.9k-line single-crate GPUI application with a clear module split:
pure CPU image math (`longcapture`, `pin_snap`, `geom`), Win32/D3D plumbing
(`capture`, `gpui_bridge`, `codec`), and GPUI views (`overlay`, `pin`, `settings`).
The pure-math half is well tested (267 passing lib tests, including pixel-exact
reconstruction tests for up/down/horizontal long capture). The weak half is
everything that crosses a thread, a GPU device, or a memory budget.

No **P0** defect was found. Five **P1** findings matter before Phase 2:

1. **Long-capture ticks are fully synchronous on the GPUI main thread.** The
   per-tick GPU readback (staging `Map`, a full-monitor tonemap), grayscale,
   coarse-to-fine SAD, stitching, status formatting, diagnostic file I/O and the
   repaint all run between two log lines on the UI thread. With the
   non-live fallback this can exceed the 150 ms poll interval and visibly stall
   the overlay.
2. **Upward stitching is O(n) per tick.** `Stitcher::prepend` allocates a new
   buffer and copies every already-stitched byte (`longcapture.rs:787-791`). A
   2000 px wide session at the 16384 px cap copies ~131 MB per upward tick; an
   8K-wide selection copies ~503 MB. The cost is quadratic over a session and
   runs on the UI thread.
3. **There is no unified image memory budget.** Caps are per-feature and
   one-dimensional (`LONG_MAX_H = 16384`, `MAX_CANVAS_SIDE = 16384`), arithmetic
   is unchecked, and allocation is infallible `vec![0u8; bytes]`. A single
   `pin_snap::compose` call may legitimately request **16384×16384×4 B = 1 GiB**
   (`pin_snap.rs:317-322`); with `panic = "abort"` an allocation failure kills
   the process.
4. **GPU device reset is only half handled.** The app drops shaders and the
   freeze swap chain on `on_gpu_reset_begin` (`app.rs:95-101`), but
   `CapturedDesktop.gpu` / `sdr_display` / `LongLive` carry no device generation,
   the DirectComposition freeze visual is recreated empty and never re-published,
   and a live WGC session is never restarted. A TDR during an overlay or a long
   capture leaves the session permanently broken until the user starts over.
5. **A static strip stops long capture indefinitely.** The matcher uses the
   bottom 32 rows (down) and the top 32 rows (up) as the overlap strip. A sticky
   footer/header occupies exactly those rows, so the SAD score is 1.0 with zero
   new rows every tick and the session makes no progress; the UI keeps saying
   "scroll the page". There is also no seam verification after a stitch.

Verification at HEAD (details in §10):

| Command | Result |
|---|---|
| `cargo fmt --check` | **FAIL** — 52 diff hunks in 16 files |
| `cargo clippy -p fastpin --all-targets` | **0 errors**, 5 unique warnings (the lib-test target re-reports 2 from the lib; 5 distinct lints) |
| `cargo test -p fastpin` | **267 passed / 1 failed** — `settings::tests::source_only_keeps_legacy_migration_literals`, caused by `README.md:29` (known, user-owned) |
| `cargo build --release --locked` | **PASS** (cached, 1.64 s), `FastPin.exe` 36,567,552 B |

CI (`.github/workflows/ci.yml:9-11`) runs `clippy -D warnings` and the same
test suite, so **CI is red at HEAD** on both counts; it never runs
`cargo fmt --check`.

No HDR, multi-monitor, DPI, GPU-reset or clipboard interop test could be
executed in this environment. Those rows are marked **Manual acceptance
pending** in §10.3 and must not be inferred from unit tests.

---

## 2. Architecture & dependency assessment

### 2.1 Module dependency (~one crate, no cycles of consequence)

```
main -> app (run/tray/self-test/once-capture)
         |-- capture (prepare_capture -> WGC | DXGI-dup -> composite -> tonemap -> BGRA)
         |-- overlay (view, annotate, composite, editor, toolbar, theme)
         |     |-- longcapture (gray / SAD / Stitcher, pure)
         |     |-- pin_snap (snap geometry, compose, crop, flatten_gaps, pure)
         |     `-- codec (WIC PNG, CF_DIB, CF_UNICODETEXT)
         |-- pin / pin_edit (floating windows, group registry)
         |-- ocr (local oar-ocr) / translate (WinHTTP + Credential Manager)
         `-- gpui_bridge, hdr, black, geom, perf, config, settings, update, migrate
```

Notable coupling: `overlay::long_tick` reaches into `app::capture_region_live`
and `app::capture_region_bgra`; `pin` reaches into `app::export_pin_pixels`;
`pin_edit` and `pin` share the `pin_snap::compose` primitive. `gpui-kit` 0.8 is
the GPUI facade (`extern crate gpui_kit as gpui`), and the crate graph resolves
`gpui-pre` / `gpui-pre-windows` from the local patch table (`Cargo.toml`
`[patch.crates-io]`, `third_party/gpui-pre`, `third_party/gpui-pre-windows`).
A vendored `third_party/gpui` 0.2.2 tree exists but is not referenced by the
patch table or `Cargo.lock`; it is dead weight for this build.

### 2.2 Thread model

| Work | Thread | Evidence |
|---|---|---|
| GPUI event loop, window update/render | UI thread | `App` / `Window` APIs |
| One-shot capture: WGC session start + frame wait | worker per monitor (`thread::spawn`), then joined | `capture/mod.rs:129-140` |
| One-shot capture: composite, tonemap, staging readback | UI thread inside `with_gpu_context` | `app.rs:113-126`, `capture/mod.rs:158-290` |
| Overlay render, magnifier readback, mosaic readback, crop-for-export | UI thread | `overlay/mod.rs:1708`, `1731`, `app.rs:129-141` |
| Long-capture poll timer | background executor timer → `this.update` marshals back to the UI thread | `overlay/mod.rs:1380-1392` |
| Long-capture tick (capture, gray, SAD, stitch, log) | **UI thread** | `overlay/mod.rs:1401-1511` |
| Annotation compositing + PNG + clipboard | detached `std::thread` (COM MTA) | `app.rs:986-1047` |
| Pin group compose/export | detached `std::thread` | `pin.rs:1351-1382` |
| OCR inference | `cx.background_spawn` | `overlay/mod.rs:1621`, `1646` |
| Translation HTTP | same background task as OCR | `overlay/mod.rs:1646-1652` |

Statics shared across threads: `GPU_SHADERS` (`d3d.rs:68`), `FREEZE_SWAP`
(`gpui_bridge.rs:19`), `FREEZE_TEXTURE` (`d3d.rs:69`), `ENGINE` (`ocr/mod.rs:29`).
All use `Mutex` with poison recovery. `PINS`/`EDITS` are thread-local
(`pin.rs:76-79`, `pin_edit.rs`), i.e. UI-thread only.

### 2.3 GPU path

- One D3D11 device owned by GPUI; the app only borrows it through
  `App::with_gpu_context`. It never creates its own device.
- Capture: WGC (preferred) → `IDirect3D11CaptureFrame` → `copy_to_owned` →
  composite (FP16 when any monitor is HDR) → `to_sdr_bgra` (GPU tonemap) →
  staging readback. DXGI Desktop Duplication is the fallback.
- Overlay freeze: a second DirectComposition visual (`freeze_visual`) below the
  GPUI UI visual, fed by a composition swap chain created in
  `gpui_bridge.rs:78-105`. The GPUI side is a local patch in
  `third_party/gpui-pre-windows/src/directx_renderer.rs:1043-1092`.
- Device loss: gpui-pre re-creates devices, swaps them on the main thread and
  bumps `gpu_generation` (`platform.rs:1469-1484`); `on_gpu_reset_begin` is the
  only app-facing hook (`third_party/gpui-pre/src/app.rs:1965`). **There is no
  reset-end hook.**
- App-side caches keyed to the device: shaders only (`GpuShaders::get_or_compile`,
  `d3d.rs:82-92`). Textures, `Gpu`, `LongLive.session`, `CapturedDesktop` are not.

### 2.4 Long-capture data flow

```
timer(150ms) on bg executor
  -> this.update -> long_tick (UI thread)
     -> capture_region_live (WGC WgcLive.take_latest -> copy_to_owned -> tonemap -> staging Map -> repack)
        or capture_region_bgra (full prepare_capture + composite + tonemap + Map)
     -> normalize_bgra (horizontal: rotate 90; vertical: borrow)
     -> bgra_to_gray
     -> classify_scroll_diag (down SAD, then up SAD only if down rejected)
     -> Stitcher::append | prepend
     -> log_diag (open+write+flush) -> cx.notify (full overlay repaint)
```

`LongCaptureState` (`overlay/mod.rs:208-231`) lives in the view and is mutated
only inside `long_tick`, i.e. on the UI thread. The export path takes the
stitcher out (`finish_long_capture`, `overlay/mod.rs:1582-1600`) and hands the
buffer to a worker thread for PNG + clipboard.

---

## 3. Findings table

Severity: **P0** = crash/data loss on the normal path; **P1** = high-impact
common path or core-feature failure; **P2** = conditional/moderate; **P3** =
robustness/polish. Effort: S < 0.5 d, M 0.5–2 d, L > 2 d.

| ID | Sev | Area | file:line | Impact | Proposed fix | Effort |
|---|---|---|---|---|---|---|
| P1-01 | P1 | Long capture / threading | `overlay/mod.rs:1401-1511`, `app.rs:150-222`, `capture/mod.rs:64-155` | GPU readback + gray + SAD + stitch + diag I/O + repaint all on the UI thread; fallback tick can exceed 150 ms and freeze the overlay | Split the tick: UI keeps capture/readback; move normalize/gray/SAD/stitch to one dedicated worker with a channel and generation tag | L |
| P1-02 | P1 | Long capture / memory traffic | `longcapture.rs:769-793`, `overlay/mod.rs:1457` | `prepend` reallocates and copies the whole buffer every upward tick; quadratic cost, up to ~500 MB copied per tick | Use a chunk/deque or keep a head reserve and `copy_within`; or store segments and flatten once on export | M |
| P1-03 | P1 | Image budget | `pin_snap.rs:262,313-322`, `longcapture.rs:703-717,736-759`, `longcapture.rs:19-25,559-590` | Up to 1 GiB canvas per call; unchecked `usize`/`u32` arithmetic; infallible `vec!` aborts on OOM (`panic=abort`) | Add a single budget module: `checked_bytes`, `alloc_bgra` with `try_reserve`, max side/pixels/bytes, typed "too large" error; route every path through it | M |
| P1-04 | P1 | GPU lifecycle | `app.rs:95-101`, `gpui_bridge.rs:19-24`, `capture/d3d.rs:142-150`, `third_party/gpui-pre-windows/src/directx_renderer.rs:309-334`, `overlay/mod.rs:1411-1435` | After a TDR the freeze visual is empty, old textures/`Gpu` are still held, live WGC never recovers; capture/export fail until the next capture | Add device generation to `Gpu`/`CapturedDesktop`; cancel overlay + long capture on reset begin; add a reset-end hook (patch) to re-publish the freeze chain; restart live WGC | M |
| P1-05 | P1 | Long capture quality | `longcapture.rs:167-174`, `overlay/mod.rs:1441-1507` | Sticky footer/header occupies the 32-row strip → SAD 1.0, 0 new rows, permanent stall with no feedback; no seam validation | Try alternate strip positions / detect "strip static, rest changed"; surface a status and offer a shorter selection; add a seam check | M |
| P2-01 | P2 | Long capture startup | `overlay/mod.rs:1260,1358-1378`, `capture/wgc.rs:190-206` | `start_live_session` busy-sleeps up to 500 ms on the UI thread when the long-capture button is pressed | Poll with a short async timer or start the session on a worker and report readiness | S |
| P2-02 | P2 | Diagnostics I/O | `overlay/mod.rs:1466-1500`, `perf.rs:116-148` | `log_diag` opens/writes/flushes `diag.log` on the UI thread every 150 ms | Buffer diag lines and flush on a worker / once per session; gate behind a config flag | S |
| P2-03 | P2 | Pin compose | `pin_snap.rs:325`, `pin.rs:1373-1376`, `pin_edit.rs:226` | Bad tile buffers are silently skipped, and `crop` failure silently drops painted marks; export failure is log-only | Return `Result`, count skipped tiles, show a user-visible error instead of a log line | S |
| P2-04 | P2 | Pin edit | `pin_edit.rs:103-126,195-231` | Backdrop/canvas compositing runs on the UI thread; the backdrop is retained for the whole edit (extra full-size buffer) | Compose off-thread on edit start/finish or reuse the already-rendered group texture | M |
| P2-05 | P2 | Pin memory | `pin.rs:366-369` | Every pin stores the BGRA `Arc<Vec<u8>>` **and** a cloned `RgbaImage` in `RenderImage` (2× pixels, 3× with GPU upload) | Build `RenderImage` from a shared/copy-on-write buffer, or drop `pixels` until an export/edit needs it | M |
| P2-06 | P2 | Translation | `translate.rs:278-298` | Response body is accumulated with no byte cap; a broken/hostile endpoint can exhaust memory (single allocation up to 4 GiB) | Cap total bytes (e.g. 4–8 MiB) and fail with a clear error | S |
| P2-07 | P2 | OCR | `ocr/mod.rs:35-45,214-227` | Global mutex held across inference (OCR and translate-OCR serialize, no cancellation); one detached 60 s timer thread per recognition | Keep the lock but add a cancellation flag/epoch; replace per-call timers with one shared idle timer | S |
| P2-08 | P2 | Codec/clipboard | `codec/mod.rs:98-130` | `width*4`, `packed_stride*height`, `header + image` unchecked in `u32`/`usize`; DIB copy doubles the export buffer for 16k images | Use the budget module's checked sizes; consider `CF_DIBV5`/PNG on the clipboard for very large images | S |
| P2-09 | P2 | Annotations | `overlay/composite.rs:1111-1131` | Watermark image is decoded with no dimension/pixel cap; a huge user image spikes memory during export | Check file size/dimensions before decode; downscale to the capture size | S |
| P2-10 | P2 | CI / hygiene | `ci.yml:9-11`, `settings/mod.rs:1954`, `README.md:29` | CI fails at HEAD: 5 clippy warnings under `-D warnings`, and the source scan test fails on the README credit line (exemption marker is `二次开发`, line says `二次vibe coding开发`); CI never runs `fmt` | Add `cargo fmt --check`; fix or explicitly exempt the 5 lints; decide the README/test exemption with the user (do not silently edit README) | S |
| P2-11 | P2 | Long capture tests | `longcapture.rs:810-1659` | No deterministic tests for sticky header/footer, repeated table rows, dynamic content, fast scroll, 1–2 px steps, seam continuity | Add the scenario matrix from §7.2 (synthetic frames + stitcher end-to-end) | M |
| P2-12 | P2 | Long capture errors | `overlay/mod.rs:1427-1433` | Size-mismatched or failed frames return `true` forever; no error counter/status, session can spin silently | Count consecutive failures, surface a status after N, and stop/restart the live session | S |
| P3-01 | P3 | Long capture status | `overlay/mod.rs:1473-1477` | A stitch width mismatch is reported as "已达最大高度" (height cap) and ends the session | Distinguish width mismatch / budget hit / capture loss in `LongCaptureState.status` | S |
| P3-02 | P3 | Clipboard | `codec/mod.rs:100-101` | `packed_stride = width * 4` and `image_size = packed_stride * height` overflow for extreme sizes | Checked arithmetic from the budget module | S |
| P3-03 | P3 | Pin move | `pin.rs:492-504` | `DeferWindowPos` error returns without `EndDeferWindowPos`, leaking the batch handle | Call `EndDeferWindowPos` (or `CloseHandle`-equivalent) on the error path | S |
| P3-04 | P3 | Pin zoom | `pin.rs:1104-1112,1276-1300` | Group members are scaled without their own MIN/MAX zoom clamp; mixed-zoom groups can exceed limits | Clamp each member and derive the common factor from the clamped values | S |
| P3-05 | P3 | OCR input | `ocr/mod.rs:229-246` | `to_rgb` allocates 3 B/px with no cap (a 16k² selection ≈ 786 MB) | Route through the image budget; downscale before OCR if over the limit | S |
| P3-06 | P3 | Translation config | `translate.rs:223` | `timeout_seconds * 1000` can overflow `u32` before the cast to `i32` | Clamp the config value (e.g. 1..=120 s) | S |
| P3-07 | P3 | Long capture gen | `overlay/mod.rs:1406,1438` | Generation is checked only before the capture; no result tag/cancel token; safe today only because everything is single-threaded | Re-check `gen` after capture and tag any future worker result with it | S |
| P3-08 | P3 | Pin buffers | `pin.rs:1942-1954`, `pin.rs:1413-1430` | `repack_opaque` slices without validating `pixels` length (panic on short buffer); ✓ button clamp can overlap on a < 28 px pin | Validate length; clamp the button inside the client rect | S |

---

## 4. Task 1 — Long screenshot

### 4.1 What runs on the GPUI main thread

`spawn_long_poll` (`overlay/mod.rs:1380-1392`) only moves the **timer** to the
background executor. Every 150 ms (`LONG_POLL`, `overlay/mod.rs:255`) it calls
`this.update(...)`, which runs `long_tick` on the UI thread. Everything inside
`long_tick` is therefore UI-thread work:

| # | Work | Evidence | Complexity |
|---|---|---|---|
| 1 | WGC `take_latest` + `copy_to_owned` (GPU copy) | `capture/wgc.rs:210-224`, `overlay/mod.rs:1412-1423` | O(W·H) GPU copy of the monitor |
| 2 | `to_sdr_bgra` (full-monitor blit/draw) + `Map` staging + `to_vec` | `capture/mod.rs:309-322`, `capture/d3d.rs:192-223,318-358` | O(W·H) GPU + memcpy, **GPU sync on `Map`** |
| 3 | `repack_tight` (row copy when RowPitch ≠ `w*4`) | `app.rs:170-181` | O(W·H) copy |
| 4 | `normalize_bgra` → `rotate_bgra90` for horizontal sessions | `longcapture.rs:559-607` | O(W·H) per-pixel 4-byte copy (vertical: borrowed, zero copy) |
| 5 | `bgra_to_gray` | `longcapture.rs:19-25` | O(W·H) + one `Vec<u8>` allocation per tick |
| 6 | `classify_scroll_diag` → down SAD (coarse+refine), then up SAD only if down rejected, then `frame_change_ratio` on reject | `longcapture.rs:451-509` | coarse `O(W·H/16)`, refine `O(5·W·strip)`, up doubles the paths; change ratio `O(W·H/4)` |
| 7 | `Stitcher::append` / `prepend` | `longcapture.rs:736-793` | append O(new rows) (+ realloc), prepend **O(total buffer)** |
| 8 | `format!` status + `cx.notify` → full overlay repaint | `overlay/mod.rs:1483-1509` | O(render), not measured |
| 9 | `log_diag` (open file, write, flush) | `overlay/mod.rs:1466-1500`, `perf.rs:116-148` | synchronous disk I/O per tick |

Measured only for step 6 (synthetic data, **unoptimized test profile**):
`align 2000x1000: full sweep 670.16 ms, coarse-to-fine 53.34 ms`
(`longcapture.rs:1512-1533`, run with `--nocapture`). The release-profile cost
was **not measured**, and steps 1-3, 5, 7, 8 have no measurement at all.
No per-tick number in the code covers the stitch or the repaint: `elapsed_ms` is
computed at `overlay/mod.rs:1449`, **before** `append`/`prepend` at 1456-1460.

### 4.2 Can gray + SAD move to a background worker?

Yes. They are pure functions over `(prev_gray, gray, w, h, strip)` and the
`Stitcher` is a plain owned struct — no GPUI types, no COM. The only part that
must stay on the UI thread is `with_gpu_context` (the shared D3D11 context is
single-threaded) and `cx.notify`.

Recommended split: UI thread does steps 1-3 and sends
`(generation, px, w, h, axis)` through a channel; one long-lived worker owns
`prev_gray` + `Stitcher` and returns `(generation, accepted, is_up, new_rows,
score, ratio, maxed)`. The UI keeps only a status string and a final buffer
handoff. This also moves step 7 off the UI thread and makes P1-02 less painful.

Holding mutable GPUI state across frames: today `LongCaptureState` holds the
stitcher and previous frame and is mutated only in the UI-thread tick. That is
correct but fragile — it depends on the single-threaded assumption, not on an
explicit ownership boundary.

### 4.3 Generation, cancellation, session lifetime

- Increment: `long_seq` is bumped on every start (`overlay/mod.rs:1308-1310`)
  and on reset (`591`).
- Check: `long_tick` returns `false` when `state.gen != gen`, `maxed`, or the
  phase is no longer `LongCapturing` (`1406-1408`).
- Cancellation: `cancel_long_capture` takes `self.long` (`1603-1610`), so the
  poll sees `None` on its next wake. There is no explicit cancellation token
  and no join handle; `spawn_long_poll` is detached (`1391`).
- End: `finish_long_capture` takes the state, converts pixels, exports, and
  calls `finish` (`1582-1600`); `finish` also sets `long = None` (`1098`).
- Worst-case stop latency is one poll interval (150 ms) after cancel/finish.

### 4.4 Can an old worker pollute a new session?

**Not today**, because everything runs on the UI thread: the generation check
happens at the top of the tick, and `state` is re-borrowed from `self.long`
after the capture (`1438`), so a cancel/start cannot interleave inside a tick.
Two residual hazards:

1. The generation is **not re-checked after the capture**. If the capture ever
   becomes async (or the tick is split), a stale frame can be appended to the
   new session. Any worker split must carry `gen` in the result and check it
   before mutating the stitcher.
2. `switch_long_axis` replaces the stitcher and `prev_gray` under the **same**
   generation (`1550-1578`). Safe while synchronous; a worker would need an
   epoch, not just a session id.

### 4.5 Are up / down / horizontal all present?

Yes:

- Down: `align_vertical` (`longcapture.rs:101-118`), bottom strip,
  `offset_y = best_y + strip` (append `[offset_y..h)`).
- Up: `align_vertical_up` (`127-144`), top strip, `offset_y = best_y` (prepend
  `[0..best_y)`); covered by `pure_up_scroll_reconstructs_source_above_the_start`
  and `down_then_up_scroll_keeps_content_and_grows_height`.
- Horizontal: frames are rotated 90° on capture (`rotate_bgra90`,
  `longcapture.rs:559-573`, used by `normalize_bgra`) and back 270° on export
  (`576-590`); axis inference is `width >= 2*height` (`547-553`); a manual
  toggle resets the session (`overlay/mod.rs:1550-1578`). Pixel-exact tests:
  `longcapture.rs:1640-1657`.

### 4.6 Existing per-stage latency points

| Stage | Measured today |
|---|---|
| Whole tick up to the SAD decision | `elapsed_ms`, `overlay/mod.rs:1402,1449`, logged per tick |
| Capture / WGC / readback | only `PerfSession` marks T3-T5 in the one-shot path (`capture/mod.rs:233,273,276`); the long-capture path creates a `PerfSession` and discards it (`app.rs:157-158`) |
| gray / down SAD / up SAD | **not measured** |
| append / prepend | **not measured** (excluded from `elapsed_ms`) |
| notify / repaint | **not measured** |
| export (annotate, DIB, PNG) | **not measured** |
| dropped/late ticks | **not measured** |

Suggested additions: `t_capture`, `t_gray`, `t_sad_down`, `t_sad_up`,
`t_stitch`, `t_notify` per tick; a per-session p50/p95/max summary; a
late-tick counter when a tick exceeds `LONG_POLL`.

---

## 5. Task 2 — Image memory budget

### 5.1 What exists

Only two one-dimensional caps:

- `LONG_MAX_H = 16384` (`overlay/mod.rs:260`, enforced in `Stitcher`).
- `MAX_CANVAS_SIDE = 16384` (`pin_snap.rs:262`, enforced per side in `compose`).

There is **no** max pixel count, max byte count, process-wide in-flight budget,
checked arithmetic, or fallible allocation anywhere. `Result` is used for GPU
calls but not for buffer sizes.

### 5.2 Allocation / copy paths

| Path | file:line | Bound | Arithmetic / failure |
|---|---|---|---|
| `Stitcher::new` | `longcapture.rs:703-717` | width unbounded, height ≤ 16384 | `width*4*first_frame_height` unchecked; `Vec::with_capacity` + `resize` abort on OOM |
| `Stitcher::append` | `736-759` | `total + new_rows <= max` | `frame_width*4*frame_height` unchecked; `extend_from_slice` may realloc-copy |
| `Stitcher::prepend` | `769-793` | same | new `Vec` of `pixels.len()+bytes`, full copy each tick |
| `bgra_to_gray` | `19-25` | none | `width*height` unchecked |
| `rotate_bgra90/270` | `559-590` | none | `width*height*4` unchecked; silently returns zeros for a short input |
| `pin_snap::compose` | `313-345` | side ≤ 16384 | up to **1 GiB**; `vec![0u8; row*ch]`; `imageops::resize` allocates `dw*dh*4` per tile |
| `pin_snap::crop` | `371-388` | canvas bounds | `Vec::with_capacity` (row*height unchecked) |
| `pin::render_image` | `366-369` | none | clones the entire pixel buffer into `RgbaImage` |
| `pin::repack_opaque` | `1942-1954` | none | slices without a length check (panic on short input) |
| `pin_edit::start/finish` | `103-126,195-231` | via `compose` | backdrop + canvas + per-tile crops alive at once |
| `codec::encode_png_bgra` | `27-58` | none | WIC + `GlobalSize` copy of the encoded stream |
| `codec::set_clipboard_dib` | `98-130` | none | `u32` `packed_stride*height` overflow; full DIB copy |
| `composite::load_watermark_image` | `1111-1131` | none | decodes an arbitrary file, then allocates a premultiplied copy |
| `ocr::to_rgb` | `229-246` | none | 3 B/px |
| `app::export_pixels_async` | `986-1010` | none | unbounded detached threads, each holding px + DIB + PNG |

### 5.3 Findings

- **Silent truncation**: `compose` skips a tile whose buffer length disagrees
  with `w*h*4` (`pin_snap.rs:325`); `crop(..)?` in `flatten` silently drops the
  marks for that tile (`pin_edit.rs:226`). No error reaches the user.
- **Overflow risk**: all size math is `usize`/`u32` multiplication. On 64-bit
  `usize` the realistic Windows sizes do not overflow, but `set_clipboard_dib`
  does its math in `u32` (`codec/mod.rs:100-101`), which is a real wrap for
  extreme values.
- **`unwrap`**: the only `unwrap` on this path is `trim_png`'s
  `try_into().unwrap()` (`codec/mod.rs:67`), which is guarded by the `i+12 <=
  len` loop and is safe.
- **Abort, not error**: `vec![0u8; n]` aborts the process on allocation failure
  under `panic = "abort"`; there is no `try_reserve` path.

### 5.4 Unified entry point

Add one module (e.g. `src/budget.rs`) with:

```rust
pub const MAX_SIDE: usize = 16384;
pub const MAX_PIXELS: usize = 64 * 1024 * 1024;   // tune
pub const MAX_BYTES: usize = 512 * 1024 * 1024;   // tune
pub fn checked_bytes(w: usize, h: usize, bpp: usize) -> Option<usize>;
pub fn alloc_bgra(w: usize, h: usize) -> Result<Vec<u8>>; // try_reserve_exact
pub fn check_canvas(w: i32, h: i32) -> Result<()>;
```

Route through it: `Stitcher::{new,append,prepend}`, `bgra_to_gray`,
`rotate_bgra*`, `pin_snap::compose/crop`, `pin::render_image`,
`pin_edit::{start,finish}`, `codec::set_clipboard_dib`, `ocr::to_rgb`,
`composite::load_watermark_image`, `app::export_*`. Surface the error in the
panel/status instead of `continue`/`log_app`.

---

## 6. Task 3 — GPU lifecycle

### 6.1 Explicit lifecycle state

There is **no** app-level GPU state machine. What exists:

- `cx.gpu_generation()` read once per capture (`app.rs:109`) and used to key
  shader compilation (`d3d.rs:82-92`).
- `cx.on_gpu_reset_begin` drops `GPU_SHADERS` + `FREEZE_TEXTURE`
  (`d3d.rs:124-127`) and `FREEZE_SWAP` (`gpui_bridge.rs:22-24`) from
  `app.rs:95-101`.
- No reset-end callback exists in the patched gpui-pre
  (`third_party/gpui-pre/src/app.rs:1965` is the only one).
- No `GetDeviceRemovedReason` call anywhere in `crates/fastpin` (grep found
  none); device loss is only observed indirectly as an HRESULT error.

### 6.2 Do resources carry a device generation?

No. `Gpu` (`d3d.rs:142-150`) stores only COM pointers; `CapturedDesktop`
(`capture/mod.rs:27-39`) stores `gpu` + `sdr_display`; `LongLive`
(`overlay/mod.rs:243-249`) stores `gpu` + the WGC session. Only `GpuShaders`
carries `generation` (`d3d.rs:72-79`). The clone of a `Gpu` from generation N
is still usable after generation N+1 lands, and nothing checks.

### 6.3 Can old resources be used after a rebuild?

Yes, and this is the P1-04 failure mode:

- The overlay keeps `freeze_swap: Option<IDXGISwapChain1>`
  (`overlay/mod.rs:124`, set at `573`, re-attached only in `attach_and_show`,
  `628-633`). On device loss gpui-pre recreates `DirectComposition` and calls
  `set_swap_chain` for the **UI** chain only
  (`third_party/gpui-pre-windows/src/directx_renderer.rs:309-334`); the new
  `freeze_visual` is created empty (`1043-1068`) and is never re-published.
  Result: the frozen background disappears; the overlay shows the live desktop
  underneath its UI.
- `cap.sdr_display` is a texture on the old device. `crop_from_app` /
  `selection_pixels` / magnifier / mosaic all call `read_bgra_region` with the
  **new** device context (`app.rs:137`, `overlay/mod.rs:1708,1731`), i.e. a
  cross-device `CopySubresourceRegion`. It fails; export, OCR, pin and
  magnifier stop working for that session.
- `LongLive.session` was created with `gpu.winrt_device()` from the old device
  (`capture/wgc.rs:158-159`). After a reset `take_latest` fails on every tick
  and `long_tick` swallows it with `Ok(Some(_)) | Err(_) => return true`
  (`overlay/mod.rs:1428`), so the session spins forever with a frozen counter.

### 6.4 Recovery / graceful shutdown

- Window close: overlay hooks `on_window_should_close` and cancels
  (`overlay/mod.rs:656-663`); pin windows forget their registry entry
  (`pin.rs:346-350`); `close_group` removes every member
  (`pin.rs:730-744`). This part is sound.
- Device reset: no recovery. The correct sequence would be: on
  `gpu_reset_begin`, mark `Gpu`/`CapturedDesktop`/`LongLive` invalid, cancel any
  long capture, clear the freeze visual, and on reset end (needs a patch hook)
  re-create the freeze chain from a fresh capture or close the overlay.
- DComp freeze swap chain reuse checks only size (`gpui_bridge.rs:52-72`), not
  device identity, so a stale chain is reused after a reset if the generation
  did not clear `FREEZE_SWAP` (it does today, via `drop_freeze_swapchain`).

### 6.5 Rendering architecture constraints

- The app must treat the GPUI device as borrowed and single-threaded: all
  `with_gpu_context` calls happen on the UI thread.
- The freeze frame is a **separate** swap chain with
  `B8G8R8A8_UNORM` + premultiplied alpha + flip-sequential
  (`gpui_bridge.rs:87-103`); it must match the captured size exactly.
- Freeze visual z-order is patched: `root_visual.AddVisual(freeze, false, None)`
  then `AddVisual(ui, true, Some(freeze))` (`directx_renderer.rs:1055-1058`).
  Any re-publish must keep that order.
- The app must not create a second D3D device for readback; the WGC frame is
  copied into an app-owned texture on the shared device (`copy_to_owned`,
  `d3d.rs:170-189`), which is why capture survives until the next reset.

---

## 7. Task 4 — Long-screenshot quality

### 7.1 Existing algorithm and tests

Matcher: grayscale (C# weights) → SAD over a bottom strip (down) or top strip
(up); coarse 4× pass then full-resolution refinement within ±2 coarse steps;
acceptance is `score ≥ max(0.90, floor)` with `runner_up/best > 2.0`, or
`score ≥ 0.78` with ratio `> 2.5` (`longcapture.rs:53-94`). Runner-up ignores
offsets within `strip` of the winner (`277-283`, `378-381`). `MIN_NEW_ROWS = 2`
(`402`).

Tests cover: weights, identical frames, down/up by k, unrelated frames, blank
frames, tiny frames, early-exit SAD equivalence, budget, append/prepend
bookkeeping, pure up-scroll reconstruction, down-then-up, coarse-vs-full
agreement, noisy acceptance vs unrelated rejection, down-pass skip, rotation
round-trip, axis inference, horizontal right/left pixel-exact reconstruction
(`longcapture.rs:838-1657`). `overlay` itself has **no** long-capture test.

### 7.2 Deterministic-test gaps

| Scenario | Current coverage | Gap / risk | Suggested test |
|---|---|---|---|
| Blank page | `ambiguous_matches_are_rejected` (`917`) | none at session level | run 5 blank frames through classify+Stitcher, assert 1 segment |
| Repeated table rows (period ≤ strip) | none | runner-up ignores ±strip → a wrong offset can win; exactly periodic content is rejected (0/0), near-periodic is the risk | synthetic rows with period 16, strip 32, assert accepted offset == true one or reject |
| Dynamic content (video/cursor) | none | score may fall below floor → no progress | add a moving block; assert it never appends a wrong offset |
| Fixed header/footer | none | **P1-05**: strip is static → 0 rows forever | sticky 40 px footer; assert new rows still accepted or a clear "stalled" status |
| Fast scroll | only `changed` metric, no test | outrun strip → no accept, status only | scroll by `height`, assert `report.changed > 0.45` and no mutation |
| Small step (1–2 px) | none | `MIN_NEW_ROWS` boundary; repeated 1 px steps must accumulate, not drop | 3 consecutive 1 px steps; assert total height grows by 3 or 0, never duplicates |
| Up / down | strong (`1171`, `1325`) | none | — |
| Horizontal | strong (`1640`, `1652`) | none | — |
| Content changes during capture (window swap) | `unrelated_frames_are_rejected` (`902`) | only at `align_vertical` level | swap frame mid-session; assert session stops/reports instead of appending |
| Seam continuity | implicit in reconstruct tests | no explicit seam check after `append` | compare last N stitched rows with the frame's overlap rows |

### 7.3 Mis-stitch risk points and confidence additions

1. **Strip blind spot**: offsets within `strip` are excluded from the runner-up,
   so a repeated pattern with period ≤ 32 has no rival by construction. Add a
   second, independent check (e.g. SAD on a shifted strip, or a full-frame
   `frame_change_ratio` restricted to the non-strip area).
2. **Down-first bias**: an up-scroll is only matched after the down pass is
   rejected; repeated content on the wrong side could satisfy down with > 2 new
   rows. Require the two directions to disagree before accepting (if both
   accept, prefer the one with the larger margin).
3. **No seam validation**: the score describes the strip match, not the seam
   the user sees. Add `seam_sad` (compare 2-4 rows above the seam with the
   stitched rows before it) and log it next to `score`/`runner_up`.
4. **No confidence surfacing**: `ScrollReport` has `score`, `ratio`, `changed`,
   but the user only sees a status string. A session that keeps landing in the
   medium band at ratio ~2.6 is a warning sign; count consecutive low-margin
   accepts and stop with a message.
5. **Static strip**: detect `score == 1.0 && new_rows == 0 && changed > 0` and
   move the strip inward (e.g. 3/4 height) for the next tick, or tell the user
   to exclude the sticky band from the selection.

---

## 8. Task 5 — Pin system

### 8.1 Window lifecycle and group registry

`PINS` is a UI-thread `RefCell<Vec<PinEntry>>` (`pin.rs:76-79`). `show_pin`
creates a borderless topmost window, registers it with `group = id`, and hooks
`on_window_should_close` (`294-364`). `close_group` removes every member and
forgets all ids (`730-744`). `detach` gives the tile a fresh group id
(`438-447`); `link` merges groups (`422-436`). `forget` only drops the id
(`449-451`), so an OS-level close of one member leaves the others in the group
(a reasonable behavior, but the group id then points at a dead pin — harmless).

No leak was found in the normal paths; the `Pending` ✓ is cleared when either
side moves (`clear_pendings_targeting`, `1442-1454`).

### 8.2 Snap geometry

`find_snap` (`pin_snap.rs:124-202`) considers four sides, requires a shared
edge of `MIN_SHARED_EDGE = 16 px`, rejects stacking above `MAX_OVERLAP = 0.5`,
rejects candidates that overlap any tile of the **target group** (so only
exposed edges snap), and picks the smallest displacement. `SNAP_ENTER=12` /
`SNAP_EXIT=20` give hysteresis (`pin.rs:86-89`). Geometry is in physical pixels
throughout; `IRect` uses `i32`, which is safe for the virtual desktop.

`are_adjacent` (`206-215`) requires a shared edge and no overlap; `scale_edges`
(`220-227`) keeps shared edges shared by rounding each coordinate, which is the
right primitive for zoom. Tests cover the main cases (`pin_snap.rs:418-511`,
`pin.rs:2179-2240`).

### 8.3 Group move / resize / zoom

- Move: one `BeginDeferWindowPos` batch for all members (`move_windows`,
  `pin.rs:480-508`); the drag target comes from the cursor minus the press
  offset, in hardware pixels (`1117-1198`).
- Resize: `resize_rect` keeps aspect ratio, clamps the min side and
  `MAX_RESIZE_ZOOM`, and anchors the opposite corner (`183-265`); the group
  follows with the same factor about the same anchor (`1087-1115`).
- Zoom: `zoom_group` clamps the grabbed tile to `[floor, MAX_ZOOM]`, then
  applies the resulting factor to members (`1276-1300`).

### 8.4 Compose, transparent gaps, big images

- `compose` lays tiles at native resolution when the group has a uniform zoom,
  else at screen resolution (`layout`, `274-299`); it resamples per tile with
  `Triangle` (`331-341`), blits with clipping (`349-367`), and leaves gaps
  transparent (`vec![0u8; …]`).
- `flatten_gaps` replaces alpha-0 pixels with white for the clipboard DIB
  (`pin.rs:1377-1379`, `pin_snap.rs:392-399`); the PNG keeps transparency.
- Big image: `MAX_CANVAS_SIDE = 16384` is the only limit (P1-03); `compose`
  returns `None`, `export` logs and returns with **no user-visible error**
  (`pin.rs:1373-1376`).

### 8.5 Geometric consistency / boundary defects

1. `flatten` recomputes `tile_dest` and crops from the canvas; a ±1 px rounding
   difference between `compose` (blit clipping) and `crop` (strict bounds) can
   drop a tile's marked pixels (`pin_edit.rs:226`).
2. `compose` silently skips a tile with a bad buffer (P2-03).
3. Group members are not clamped individually during resize/zoom (P3-04).
4. `repack_opaque` panics on a short buffer instead of returning an error
   (P3-08).
5. The ✓ placement clamps to `(w-28).max(6)` (`pin.rs:1421-1428`); on a < 28 px
   pin the ✓ can sit on top of the ✕. Minor.

---

## 9. Task 6 — OCR and translation

### 9.1 OCR

- **Fully local**: `ocr/mod.rs:1` ("Screenshots never leave the machine"); the
  only inputs are the three model files (`PP-OCRv6_det_small.onnx`,
  `PP-OCRv6_rec_small.onnx`, `ppocrv6_dict.txt`) resolved from settings /
  `FASTPIN_MODELS` / `models/v6` / the repo fallback (`185-212`). `DirectML.dll`
  is the local GPU backend shipped next to the exe. No network code in the
  module.
- **Model lifecycle**: one global `Mutex<Option<Engine>>` (`29`); built on first
  use (`36-41`); dropped after 60 s idle by a detached timer thread spawned per
  recognition (`214-227`). Multiple recognitions spawn multiple timers; each
  wake-up re-checks `last_used` before dropping.
- **Concurrency**: the mutex is held across `predict` (`35-45`), so OCR and the
  OCR half of translate serialize. That bounds memory but means a hung
  inference blocks every later request; there is no cancellation or timeout.
- **Error reporting**: `recognize` returns `Err` with the ORT error; the overlay
  shows `识别失败` in the panel (`1624-1630`). Foreground work is off the UI
  thread via `background_spawn` (`1621`).
- **Input size**: `to_rgb` allocates 3 B/px with no cap (P3-05).

### 9.2 Translation

- **Protocol**: OpenAI-compatible POST built by hand (`translate.rs:40-58`);
  WinHTTP, one request per call. `endpoint` accepts host / `/v1` / full path
  (`137-146`).
- **Timeouts**: all four WinHTTP phases set to the same budget
  (`WinHttpSetTimeouts`, `223-224`); config value clamped to ≥ 5 s (`54`), but
  `*1000 as i32` can overflow for absurd values (P3-06).
- **Response size**: the read loop trusts `WinHttpQueryDataAvailable` and grows
  `payload` without a cap (`278-298`) — P2-06.
- **API key**: Windows Credential Manager (`PasswordVault`) under
  `FastPin.Translation` / `apiKey` (`61-91`), with a best-effort migration from
  `Zshot.Translation` (`97-134`). The key is never logged and never written to
  `settings.json`. It is sent as a `Bearer` header; an `http://` base URL would
  send it in cleartext (user-configurable, worth a UI warning).
- **Cancellation / errors**: the HTTP call is blocking inside a background task;
  closing the overlay does not cancel it, but the result is dropped by the
  failed `this.update`. Non-2xx responses return a status-only message
  (`300-304`); the body (bad key vs quota) is discarded.
- **Local-only boundary**: OCR is local; translation necessarily sends the
  recognized text to the user's endpoint. That is the documented design, not a
  defect.

---

## 10. Task 7 — Tests, CI and acceptance

### 10.1 Actual command results at HEAD

Environment: `cargo 1.99.1`, `rustc 1.99.1`; `TEMP`/`TMP` set to
`target/test-temp` for the test run.

**`cargo fmt --check` — FAIL (exit 1), 52 hunks in 16 files**

| File | Hunks |
|---|---|
| `src/settings/mod.rs` | 13 |
| `src/platform/tray_menu.rs` | 7 |
| `src/settings/widgets.rs` | 5 |
| `src/pin.rs` | 5 |
| `src/overlay/annotate.rs` | 4 |
| `examples/gen_icon.rs` | 3 |
| `src/brand_icon.rs` | 3 |
| `src/platform/tray.rs`, `src/overlay/composite.rs`, `src/migrate.rs` | 2 each |
| `src/translate.rs`, `src/settings/system.rs`, `src/settings/model.rs`, `src/overlay/toolbar.rs`, `src/ocr/mod.rs`, `src/app.rs` | 1 each |

(No file was reformatted; this is a check only.)

**`cargo clippy -p fastpin --all-targets` — 0 errors, 5 unique warnings**

(The lib-test target reports 5 warnings, 2 of which duplicate the lib target;
the distinct lints are the five below.)

| Lint | file:line |
|---|---|
| `derivable_impls` | `src/overlay/composite.rs:1034` |
| `too_many_arguments` (9/7) | `src/config.rs:153` |
| `field_reassign_with_default` ×3 | `src/config.rs:479`, `src/config.rs:583`, `src/config.rs:607` |

CI runs `-D warnings` (`ci.yml:9`), so these fail CI.

**`cargo test -p fastpin` — 267 passed / 1 failed (1.57 s)**

```
---- settings::tests::source_only_keeps_legacy_migration_literals stdout ----
panicked at crates\fastpin\src\settings\mod.rs:2012:9:
old product name outside the migration constants:
D:\code\FastPin\README.md:29: FastPin 最初基于 Zshot 二次vibe coding开发。
```

Root cause: the test exempts README lines containing `二次开发`
(`settings/mod.rs:1954`), but the user-written line says `二次vibe coding开发`,
so the marker does not match. This is the known, user-owned README noise; per
the standing instruction it must not be "fixed" by adapting the test or editing
the README without an explicit decision. It is still a **CI-blocking** failure
because CI runs the same test on a clean checkout.

**`cargo build --release --locked` — PASS**

```
Finished `release` profile [optimized] target(s) in 1.64s
target/release/FastPin.exe  36,567,552 bytes
```

### 10.2 CI vs the four local commands

| Local | CI (`ci.yml`) | Difference |
|---|---|---|
| `cargo fmt --check` | not run | formatting drift is invisible to CI (52 hunks today) |
| `cargo clippy -p fastpin --all-targets` | `cargo clippy --workspace --all-targets --locked -- -D warnings` (line 9) | CI promotes warnings to errors → 5 current warnings fail the job; `--locked` can also fail on a stale lockfile (not the case today) |
| `cargo test -p fastpin` | `cargo test --workspace --locked` (line 10) | same failing test; no `TEMP` override; CI has no GUI, so all UI tests are unit-level only |
| `cargo build --release --locked` | same (line 11) | equivalent |

Neither CI nor the local commands exercise: real WGC/DXGI capture, HDR
tonemapping on hardware, DirectComposition freeze, device reset, multi-monitor,
DPI changes, clipboard interop, OCR models, or the HTTP path.

### 10.3 Windows manual acceptance matrix

Status values: **Automated** = covered by a unit test that actually runs;
**Manual pending** = must be executed on Windows hardware; **Known limitation** =
current code cannot satisfy it. Nothing in this table was executed in this
round except the automated rows.

| # | Area | Scenario | Expected | Status |
|---|---|---|---|---|
| 1 | SDR | 100% single monitor, capture + save + copy | exact colors, no washout | Manual pending |
| 2 | SDR | `sdr_white = 80` identity path | no white lift drift | Automated (WARP golden, `d3d.rs:594`) |
| 3 | HDR | HDR on, scRGB FP16, highlight content | highlights not clipped, midtones match | Manual pending |
| 4 | HDR | SDR white 200 vs 400 nits | tone curve follows the display | Manual pending |
| 5 | HDR | HDR + SDR monitors in one virtual desktop | composite format correct, no banding | Manual pending |
| 6 | Multi-monitor | monitor left/above primary (negative origin) | overlay, selection, pin land on the pixels | Manual pending |
| 7 | Multi-monitor | 100% + 150% + 200% mixed DPI | selection/crop/pin sizes correct | Manual pending |
| 8 | DPI | drag a pin across monitors / change scale at runtime | no size jump, no seam drift | Manual pending |
| 9 | GPU reset | TDR while idle, then capture | tray survives, next capture works | Manual pending |
| 10 | GPU reset | TDR while overlay is open | overlay recovers or closes cleanly | Manual pending (P1-04) |
| 11 | GPU reset | TDR during long capture | session stops with a message, next capture works | Manual pending (P1-04) |
| 12 | GPU reset | driver update / device removal | no stale-resource error dialogs | Manual pending |
| 13 | Clipboard | paste into WeChat / Office / Paint / Chrome | top-down content correct, alpha acceptable | Manual pending |
| 14 | Clipboard | copy-only hotkey | clipboard set, no file written | Automated (`overlay/mod.rs:2845`) |
| 15 | Long capture | blank page, 5 ticks | exactly one segment, no duplicated rows | Automated (`ambiguous_matches_are_rejected`) |
| 16 | Long capture | table with repeated rows | no drift / duplicated rows | Manual pending + test gap |
| 17 | Long capture | sticky header/footer page | progress continues or a clear message | Known limitation (P1-05) |
| 18 | Long capture | fast scroll beyond the strip | warning, no corrupted stitch | Manual pending |
| 19 | Long capture | 1–2 px slow scroll | no seam duplication, rows accumulate | Manual pending |
| 20 | Long capture | up / down / horizontal | pixel-exact reconstruction | Automated (`longcapture.rs:1289,1325,1640,1652`) |
| 21 | Long capture | 4K region, 16384 px cap | stops at the cap, exports a valid PNG | Manual pending |
| 22 | Long capture | 150% DPI selection | crop offsets correct | Manual pending |
| 23 | Pin | snap / stitch / detach / group move | seams close, ✓ on the seam | Automated geometry + Manual |
| 24 | Pin | transparent gaps | PNG transparent, clipboard white | Manual pending |
| 25 | Pin | 16k-tall pin / large group | no OOM, clear error instead of abort | Known limitation (P1-03) |
| 26 | OCR | models missing / offline | clear error, no network access | Automated (presence) + Manual |
| 27 | Translate | timeout / bad key / huge body | error shown, UI stays responsive | Manual pending |
| 28 | Translate | key stored / migrated | Credential Manager entry, no key on disk | Manual pending |
| 29 | Tray | tray menu, hotkeys, exclusive fullscreen game | menu works, hotkey still fires | Manual pending |
| 30 | Startup | `--self-test`, `--once-capture` on a clean profile | exit code 0, valid output | Manual pending |

---

## 11. Performance measurements

### 11.1 What is measured today

- `PerfSession` marks T0-T7 around the **one-shot** capture and overlay show
  (`app.rs:318-321`, `capture/mod.rs:233,273,276`, `overlay/mod.rs:669,676`);
  written to `%LOCALAPPDATA%\FastPin\logs\perf.log`.
- One `elapsed_ms` per long-capture tick covering capture → SAD decision
  (`overlay/mod.rs:1402,1449`), appended to `diag.log` with score/ratio/rows.
- A benchmark harness (`--bench-overlay`, `--stress-overlay`) with
  p50/p95/p99 in `perf-summary.json` (`perf.rs:75-89`).

### 11.2 Actual numbers obtained this round

| Measurement | Value | Conditions |
|---|---|---|
| Coarse-to-fine SAD, 2000×1000, strip 32 | **53.34 ms** | `cargo test --nocapture`, **unoptimized test profile**, synthetic LCG content, this machine |
| Full naive sweep, same input | **670.16 ms** | same run (kept only as a ratio reference) |
| Test suite runtime | 1.57 s | 268 tests, debug profile |
| Release build | 1.64 s (cached) | `FastPin.exe` 36,567,552 B |

Everything else is **not measured**: release-profile SAD, grayscale, rotate,
WGC copy/tonemap/staging `Map`, stitch append/prepend, overlay repaint,
annotation compositing, PNG encode, DIB copy, OCR latency, translation latency.
No FPS or frame-drop counter exists.

### 11.3 Measurement points to add

1. Per-tick spans: `t_capture` (`take_latest` + `copy_to_owned`), `t_crop`
   (`to_sdr_bgra` + `read_bgra_region`), `t_gray`, `t_sad_down`, `t_sad_up`,
   `t_stitch`, `t_notify`, `t_log`.
2. Session summary: p50/p95/max tick, accepted/rejected counts, consecutive
   low-margin accepts, late ticks (> `LONG_POLL`).
3. Export spans: annotate / DIB / PNG, with buffer bytes.
4. Capture path: keep the existing T3-T5 marks and add the long-capture
   fallback so a 500 ms WGC wait is visible.
5. A `--bench-long` harness using a fixed synthetic page (no GUI) so the
   numbers are reproducible in CI-like environments.

---

## 12. Remaining risks

1. **Hardware matrix unverified.** HDR tonemapping, mixed HDR/SDR, multi-monitor
   negative origins, per-monitor DPI, WGC on exclusive-fullscreen apps and all
   clipboard interop are code-reviewed only.
2. **GPU reset is unrecoverable (P1-04).** A TDR during an overlay or long
   capture loses the session and may leave a window showing the live desktop.
3. **Memory is unbounded (P1-03).** A large pin group or an 8K-wide long capture
   can allocate hundreds of MB per buffer with no error path; OOM aborts.
4. **Long capture stalls on sticky bands (P1-05)** and has no seam validation;
   the unit tests cannot catch it.
5. **CI is red at HEAD** (P2-10): 5 clippy warnings and the README-driven test
   failure; `fmt` is not gated. This makes "green CI" unavailable as a
   Phase-2 gate until resolved.
6. **No measurement of the UI-thread cost** of a tick, so a regression that
   doubles the stitch or readback time would be invisible.
7. **`diag.log` I/O per tick** is a hidden synchronous cost and grows without
   rotation.
8. **Statics without generation**: `FREEZE_SWAP`, `GPU_SHADERS`,
   `FREEZE_TEXTURE` are process-wide; the shader cache is generation-keyed but
   the freeze chain is only cleared, never re-published.
9. **`pin` stores pixels twice** and the edit session retains a full backdrop;
   several pins of a 4K screenshot are enough to be noticeable.
10. **Translation is the only network path**; it has no response cap and no
    cancellation, so a bad endpoint can stall a background task and grow memory.

---

## 13. Recommended fix order (for dispatch)

1. **P1-03** image budget module + checked allocation (prerequisite for the rest).
2. **P1-01** split the long-capture tick; worker owns `prev_gray` + `Stitcher`.
3. **P1-02** replace `prepend`'s full copy with a head-reserve or segment list.
4. **P1-05** static-strip detection / alternate strip + seam validation.
5. **P1-04** device generation + cancel-on-reset + reset-end hook to re-publish
   the freeze chain; restart or stop the live WGC session.
6. **P2-01/02/12** remove the 500 ms UI busy-wait, move diag I/O off the tick,
   surface capture errors.
7. **P2-03/04/05** pin error propagation, off-thread compose, single pixel copy.
8. **P2-06/07/08/09** translation cap, OCR cancellation/idle timer, checked
   clipboard math, watermark cap.
9. **P2-10** CI: fmt gate, lint cleanup, README/test decision (requires user).
10. **P2-11** the §7.2 test matrix; then P3 polish.

No work in this round changed business code, tests, CI, assets or settings.
