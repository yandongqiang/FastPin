# FastPin Phase 1 — Implementation Report

- **Repository:** `d:/code/FastPin`
- **HEAD:** `4b039b7` (unchanged; everything below is uncommitted working-tree work)
- **Round:** Phase 1 P0/P1 implementation, verified 2026-11-12
- **Author of this file:** verification worker (read-only on business code)
- **Constraint recap:** no commit, no push, no `README.md`/`LICENSE`/CI edit, no
  repo-wide `cargo fmt` (only `cargo fmt --check`, recorded)

---

## 1. Scope & constraints

Phase 1 delivered the five P1 defects from `PHASE1_DEFECTS.md`
(`PHASE1_AUDIT_REPORT.md` found no P0). This report records what is actually in
the working tree, what is proven by automated tests, and what is still blocked.

Ground rules applied to this verification round:

| Rule | Status |
|---|---|
| No business-code change by this worker | Honoured — only this file and `PHASE1_DEFECTS.md` were written |
| No commit / no push | Honoured — `HEAD` is still `4b039b7` |
| `README.md` untouched (staged or working tree) | Honoured — `git status` shows no `README.md` entry |
| No CI edit | Honoured — `.github/workflows/ci.yml` unmodified |
| No repo-wide formatting | Honoured — `cargo fmt --check` only, output captured |
| No invented performance numbers | Honoured — every figure below is a test/harness output, otherwise "not measured" |

Scan note: the legacy-name unit test walks the whole workspace, so this report
deliberately avoids writing the retired product name. The two audit files and
`README.md:29` are the only offenders; adding it here would add new failures.

---

## 2. P1 item by item

### P1-01 — Long-capture tick ran all CPU and GPU-sync work on the GPUI main thread

**Problem.** The poll timer moved to the background executor but immediately
re-entered `this.update`, so capture, normalize, grayscale, SAD, stitching,
`format!`, `log_diag` and repaint all ran on the UI thread. On the fallback path
one tick could exceed the 150 ms interval, and nothing counted late ticks.

**Implementation.**

| What | Where |
|---|---|
| New single-thread session worker (`LongWorker`, `Job`, `Done`, `Verdict`, `Spans`) | `crates/fastpin/src/longworker.rs:288` (struct), `:300` (`new`), `:335` (`submit`), `:353` (`poll`), `:371` (`finish`), `:390` (`shutdown`) |
| One-job-in-flight gate, stale-generation and stale-epoch rejection | `longworker.rs:175` (`ResultGate`), `:205` (`issue`), `:226` (`accept`) |
| Worker-side pipeline (normalize → gray → classify → stitch → diag) | `longworker.rs:483` (`process`) |
| Module export | `crates/fastpin/src/lib.rs:19` |
| Session holds the worker instead of `prev_gray`/`Stitcher` | `crates/fastpin/src/overlay/mod.rs:256` |
| Worker created at session start with the UI-built stitcher/gray | `overlay/mod.rs:1491` |
| Tick split: drain results, skip when a job is in flight, capture on UI, submit | `overlay/mod.rs:1614` (`long_tick`), `:1672` (in-flight skip), `:1728` (`submit`) |
| Verdict application on the UI thread | `overlay/mod.rs:1740` (`apply_long_done`) |
| Late-tick counter | `overlay/mod.rs:245`, `:1673` |
| Per-stage span line in `diag.log` (`ui_ms/proc_ms/norm_ms/gray_ms/sad_ms/stitch_ms/log_prev_ms/late/copied`) | `longworker.rs:552`, `longworker.rs:597` |

**Behaviour preserved.** Same matcher, same stitch decisions, same status
strings and the same 150 ms interval (`overlay/mod.rs:283`). The UI thread still
performs the WGC readback (the D3D11 immediate context is single-threaded); it
no longer performs any pixel pass. A result whose generation or epoch no longer
matches is discarded before it can touch pixels, closing P3-07 as a side effect.

**Tests added** (7, all in `longworker.rs:670+`): end-to-end synthetic scroll
(`:749`), old-generation job discarded (`:770`), cancelled worker produces no
applicable result (`:786`), axis-switch result dropped (`:799`), one job in
flight (`:838`), duplicate/out-of-order results dropped (`:846`), span recording
(`:871`).

**Measured delta.** The only measurement that exists is the span line the test
harness prints for a 640×360 synthetic frame in the **unoptimised test profile**:
`ui=1.500 norm=0.004 gray=3.116 sad=15.866 stitch=0.222 log=0.649 proc=19.859
total=21.359 ms`. There is no before/after capture of the UI-thread stall —
release-profile tick latency is **not measured (pending Windows runtime)**.

---

### P1-02 — `Stitcher::prepend` copied the entire buffer on every upward tick

**Problem.** Each upward tick allocated a new `Vec` and copied every stitched
byte: ~131 MB per tick at 2000 px × 16384 px, ~503 MB at 8K width, on the UI
thread, quadratic over a session.

**Implementation.**

| What | Where |
|---|---|
| Head-reserve layout (`head_rows`, `head_grant`, `pixels` grows in front) | `crates/fastpin/src/longcapture.rs:1262` (struct), `:1285` (`new`) |
| Prepend writes into existing headroom; no copy while headroom lasts | `longcapture.rs:1417` (`prepend`), fast path `:1461` |
| Geometric headroom grant + single slide | `longcapture.rs:1531` (`headroom_target`), `:1470` (grant branch) |
| Headroom given back when it would breach the byte budget | `longcapture.rs:1514` (`drop_headroom`), `:1388` (append) |
| Fallible growth instead of `extend_from_slice` reallocation | `longcapture.rs:1393` (`BUDGET.try_grow`) |
| Copy accounting (`copied_bytes`, `compactions`, `allocations`) | `longcapture.rs:1544` |
| Headroom folded away exactly once at export | `longcapture.rs:1590` (`into_pixels`) |

**Behaviour preserved.** Byte-for-byte identical output; the new test compares
against a plain-`Vec` reference model after 200 prepends. `append` semantics are
unchanged apart from fallible growth, and a failed prepend leaves the buffer
untouched (same contract as before).

**Tests added** (2 net in `longcapture.rs`): `prepend_into_headroom_is_pixel_exact_and_copies_linearly`
(`:2511`), `prepend_keeps_the_pixel_pointer_while_headroom_lasts` (`:2557`).
`prepend_then_append_keeps_image_order` (`:2578`) also covers interleaving.

**Measured delta.** From the harness line of `prepend_into_headroom...`
(200 prepends, 8 px wide, 32 px tall start, 2 rows per tick):

```
P1-02 prepend x200: copied 794 rows in 3 compactions (4 allocations) vs 46200 rows before
```

≈58× fewer copied rows for this session; copies now grow with the number of
headroom re-grants (O(log n)), not with the number of ticks. Absolute MB/s is
**not measured (pending Windows runtime)**.

---

### P1-03 — No unified image budget, checked arithmetic or fallible allocation

**Problem.** Caps were per-feature and one-dimensional, size math was unchecked,
and buffers were allocated with aborting `vec![0u8; n]` under `panic = "abort"`;
`compose` could legitimately ask for 1 GiB.

**Implementation.** New module `crates/fastpin/src/image_budget.rs` (513 lines):
constants, `BudgetError`, `ImageBudget::{checked_dims, checked_bytes,
checked_len, check_image, alloc_rgba, alloc_zeroed, try_grow}` and the
process-wide `BUDGET`. Constants and the full call-site list are in §3.

**Behaviour preserved.** Limits are only tightened where the old code had *no*
limit (width in the stitcher, encoded-buffer copies) or where the old limit was
implicitly the same 16384 side (`pin_snap`, `LONG_MAX_H`, now derived from
`MAX_SIDE`). Error text is new but only replaces "too large to compose" log
lines; the successful paths are unchanged.

**Tests added:** 12 budget tests (`image_budget.rs:353+`) covering every limit,
both sides, `u32::MAX` overflow, short buffer/short stride, allocation refusal,
and the "1 GiB ceiling is preserved" check. `longcapture` gained 3 budget tests
(`:1888`, `:1905`, `:1933`), `pin_snap` converted its oversized test to the new
error (`:693`), `codec` gained `encode_png_rejects_short_and_oversized_buffers`
(`codec/mod.rs:224`).

**Measured delta.** Not a performance item; no measurement. The functional
assertion is that an over-budget request returns `Err` instead of aborting.

**Known residue (see §9):** `ocr::to_rgb`, `composite::load_watermark_image`,
`pin::render_image` and the annotation backdrops still allocate without the
budget.

---

### P1-04 — GPU device reset left captures, the freeze visual and live WGC sessions broken

**Problem.** Only the reset-*begin* callback existed; resources carried no device
generation; the DirectComposition freeze visual was recreated empty and never
re-published; a live WGC session kept polling a dead device.

**Implementation.**

| What | Where |
|---|---|
| New pure state machine + process-wide generation (`GpuLifecycle`, `GpuLifecycleState` = `Ready/Lost/Resetting/Recreated`) | `crates/fastpin/src/gpu_lifecycle.rs:59`, `:149` |
| Transition methods (never panic, invalid transitions rejected) | `gpu_lifecycle.rs:179` (`begin_reset`), `:194` (`device_lost`), `:213` (`device_recreated`), `:236` (`observe_generation`) |
| Freeze-rebuild decision (one rebuild per generation) | `gpu_lifecycle.rs:274` (`should_rebuild_freeze`) |
| Process-wide helpers used by capture code | `gpu_lifecycle.rs:289-345` (`init/snapshot/current_generation/observe_generation/is_usable/note_failed_device_operation`) |
| Module export | `lib.rs:16` |
| `Gpu` carries the shader generation and rejects every operation on a stale one | `crates/fastpin/src/capture/d3d.rs:147`, `:174` (`ensure_current`) |
| Failed D3D calls classified (device removed/reset/hung) and fed to the state machine | `d3d.rs:181` (`note_failure`), call sites `:209`, `:239`, `:277`, `:302`, `:311`, `:389`, `:409`, `:439`, `:453` |
| `CapturedDesktop` stamped; stale captures refused before readback | `capture/mod.rs:29`, `:302` (`crop_sdr_bgra`) |
| WGC start refused on a stale device; live session remembers its generation and fails fast | `capture/wgc.rs:47`, `:143`, `:222` (`take_latest`) |
| Freeze swap chain cached with its generation, stale cache discarded | `crates/fastpin/src/gpui_bridge.rs:19`, `:31` |
| `init` + reset-begin + reset-end wiring | `crates/fastpin/src/app.rs:95`, `:99`, `:105` |
| Capture path refuses to start while resetting / self-heals a missed callback | `app.rs:113` |
| Overlay: rebuild the freeze frame once per generation, re-attach, re-capture | `overlay/mod.rs:741` (`maybe_recover_gpu`), `:769` (`recover_gpu_after_reset`) |
| Overlay: a reset under a running long capture ends the session with a message | `overlay/mod.rs:1950` (`abort_long_for_gpu_reset`) |
| Reset-end hook added to the local gpui patch | `third_party/gpui-pre/src/app.rs:1972`, `third_party/gpui-pre/src/platform.rs:642`, `third_party/gpui-pre-windows/src/platform.rs:124`, `:650`, `:1499` |
| Regenerated patch files | `patches/gpui-pre-0.3.9-fastpin-overlay.patch`, `patches/gpui-pre-windows-0.3.9-fastpin-overlay.patch` |

**Behaviour preserved.** No device is created by the app; it still borrows
GPUI's device through `with_gpu_context`. Generation 0 is the normal case, so a
session without a reset behaves exactly as before (all `is_usable` checks pass).
On a reset the overlay either rebuilds its freeze frame or closes cleanly; a
long capture stops with "显示设备已重置 / 长截图已停止，请重新开始" instead of
polling forever. The live WGC session is terminated, not silently reused.

**Tests added:** 15 (`gpu_lifecycle.rs:370+`), covering every transition, the
duplicate-callback case, generation monotonicity, self-healing a missed begin,
the freeze-rebuild predicate, stale-resource checks, and HRESULT classification.
The real reset cannot be unit-tested — see §9.

**Measured delta.** Not applicable; behaviour is event-driven. **Not measured
(pending Windows runtime).**

---

### P1-05 — Static bottom/top strip stalled long capture; no seam validation

**Problem.** The matcher used only the bottom (down) or top (up) 32 rows. A
sticky footer/header sits exactly there, so the strip matched itself, zero rows
were new, the tick was rejected forever, and the bar kept saying "keep
scrolling". A wrong offset could also be accepted and silently duplicate rows.

**Implementation.**

| What | Where |
|---|---|
| Second, independent-band verification of every accepted offset | `crates/fastpin/src/longcapture.rs:409` (`verify_shift`), result type `:372` |
| Thresholds: `VERIFY_MIN_SCORE = 0.85`, `SEAM_MAX_RATIO = 1.9`, `SEAM_BASELINE_FLOOR = 6.0`, `SEAM_ROWS = 3` | `longcapture.rs:105-121` |
| Static-band detection (`STATIC_EDGE_SCORE = 0.97`) before any stitch | `longcapture.rs:98`, used in `classify_scroll_diag` `:781` |
| Report carries `seam_score`, `static_edge`, `rejected_by_verify`, `changed` | `longcapture.rs:734-780` |
| Stall tracker: `STALL_TICKS = 10`, phases `Progress/Watching/Paused/Stalled` | `longcapture.rs:903`, `:910`, `:949` (`observe`) |
| Worker returns `Verdict::{Progress,Idle,Paused,StaticEdge,FastScroll}` and `StopReason::{Stalled{static_edge},HeightCap,Budget,...}` | `longworker.rs:95`, `:111` |
| Host surfaces the reason and decimates polling while paused (`SLOW_POLL_EVERY = 4`) | `overlay/mod.rs:291`, `:1680`, `:1754`, `:1781`, `:1792` |
| Distinguishes height cap / budget / frame rejection in the status line | `overlay/mod.rs:1769`, `:1593` (`stop_long_for_budget`) |

**Behaviour preserved.** A normal scroll produces the same accepted offsets as
before (the coarse-to-fine tests are unchanged and still pass); the new check can
only *refuse* an offset, never invent one. A genuinely paused page is kept alive
at a reduced cadence instead of being killed. Pixel-exact reconstruction tests
for down/up/horizontal are unchanged.

**Tests added** (12 net): sticky footer (`:2749`), sticky header up-scroll
(`:2782`), exactly-periodic rows refused (`:2722`), repeated table rows
(`:2686`), blank frames → pause (`:2635`), 1–2 px accumulation (`:2807`), fast
scroll (`:2839`), `verify_shift` rejects a high-similarity wrong offset (`:2858`),
dynamic content inside the verified band still stitches (`:2888`), stall tracker
reaches `Paused`/`Stalled` (`:2919`), plus the up/down and left-step pixel-exact
walk tests.

**Measured delta.** No latency figure. The functional delta is that a sticky-band
page now reports `固定页眉/页脚遮挡，请缩小选区` within one tick instead of
polling forever. **Not measured (pending Windows runtime).**

---

## 3. Memory budget constants & applied paths

`crates/fastpin/src/image_budget.rs`:

| Constant | Value | Meaning |
|---|---|---|
| `MAX_SIDE` | 16384 | max width and max height of any image buffer |
| `MAX_PIXELS` | 268,435,456 (16384²) | max pixel count |
| `MAX_BYTES` | 1,207,959,552 (1 GiB + 128 MiB) | max bytes for one image buffer |
| `RGBA_BPP` | 4 | bytes per pixel for the BGRA paths |
| `BUDGET` | `ImageBudget::STANDARD` | the instance every production path uses |

`MAX_PIXELS × 4 = 1 GiB`, so `MAX_BYTES` is deliberately 128 MiB above the pixel
cap: the pixel cap is the binding limit for RGBA, and the byte cap exists to hold
the stitcher's *physical* buffer (image + headroom) and encoded PNG copies.
`BudgetError` carries the requested value and the limit for every dimension
(`TooWide/TooHigh/TooManyPixels/TooManyBytes/Overflow/ShortBuffer/ShortStride/AllocFailed`),
so status lines and logs can name the exact limit (`image_budget.rs:42`).

Applied paths (production, not tests):

| Path | Call site | What it does |
|---|---|---|
| `pin_snap::MAX_CANVAS_SIDE` | `pin_snap.rs:263` | derived from `MAX_SIDE` so the two cannot drift |
| `pin_snap::compose` | `pin_snap.rs:343-380` | canvas `alloc_rgba`, per-tile byte check, resample size check |
| `pin_snap::crop` | `pin_snap.rs:427` | checked `row × height` before `with_capacity` |
| `longcapture::bgra_to_gray` | `longcapture.rs:23` | checked bytes + fallible zeroed alloc |
| `longcapture::rotate_bgra90/270` | `longcapture.rs:1025`, `:1043`, helper `:1061` | checked frame + fallible alloc |
| `longcapture::normalize_bgra` | `longcapture.rs:1077+` | checked frame |
| `Stitcher::new` | `longcapture.rs:1285-1310` | fallible canvas alloc |
| `Stitcher::append` | `longcapture.rs:1384-1396` | whole-canvas byte check, headroom drop, `try_grow` |
| `Stitcher::prepend` | `longcapture.rs:1453-1480` | same checks, fallible headroom alloc |
| `Stitcher::headroom_target` | `longcapture.rs:1539` | caps headroom by `max_bytes / row_bytes` |
| `codec::encode_png_bgra` | `codec/mod.rs:32` | input `check_image`; encoded copy via `checked_len` + `try_reserve_exact` |
| `codec::set_clipboard_dib` | `codec/mod.rs:114` | `check_image`, `checked_bytes`, `checked_len` instead of `u32` math |
| Overlay long canvas cap | `overlay/mod.rs:286` | `LONG_MAX_H = MAX_SIDE` |
| Pin export / pin edit | `pin.rs:1373`, `pin_edit.rs:123`, `:217` | propagate `ComposeError` with size+limit into the log |

Not routed (still infallible `vec!`, listed as residue in §9):
`ocr::to_rgb` (`ocr/mod.rs:237`), `overlay/composite.rs:1122`
(`load_watermark_image`), `pin::render_image` (the second full copy, P2-05), and
the annotation backdrops in `overlay/composite.rs:588/616/724/789`.

---

## 4. Threading model after P1-01

| Work | Thread | Evidence |
|---|---|---|
| GPUI event loop, window update/render, overlay paint | UI thread | GPUI `App`/`Window` |
| Long-capture poll timer | background executor, `this.update` marshals to UI | `overlay/mod.rs:1576` |
| Long-capture **capture + GPU readback** (`capture_region_live` / `capture_region_bgra`) | UI thread (D3D11 immediate context is single-threaded; staging `Map` cannot move) | `overlay/mod.rs:1687-1714` |
| Long-capture **normalize, grayscale, SAD/verify, stitch, diag write, status formatting** | one long-lived `fastpin-long-worker` thread per session | `longworker.rs:300-315`, `:417` (`run`), `:483` (`process`) |
| Verdict application, status string, `cx.notify` | UI thread | `overlay/mod.rs:1740` |
| Export (annotate → PNG → clipboard) | detached `std::thread` (COM MTA) | `app.rs` export path, unchanged |
| Pin group compose/export | detached `std::thread` | `pin.rs:1351+`, unchanged |
| OCR / translation | `cx.background_spawn` | `overlay/mod.rs:1999+`, unchanged |

Synchronisation:

- One job in flight (`ResultGate`, `longworker.rs:175`). A tick that arrives while
  the worker is busy is skipped and counted (`overlay/mod.rs:1672`), so capture
  cadence follows the worker instead of queueing frames.
- Every job carries `(generation, epoch, seq)`; `ResultGate::accept`
  (`longworker.rs:226`) drops anything from a closed session, a replaced axis or
  an older generation before it can mutate pixels or status. This is the
  stale-result hole the audit called P3-07.
- `LongWorker::shutdown` (`longworker.rs:390`) cancels, signals, waits
  `SHUTDOWN_WAIT`, and detaches rather than blocking the UI for a full pass; the
  worker owns its `Stitcher`, so a detached thread releases the (possibly
  hundreds of MB) buffer on its own thread.
- The stitcher's first frame is still built on the UI thread so an over-budget
  region is refused before the capture phase starts (`overlay/mod.rs:1482-1513`).

---

## 5. GPU lifecycle states & generation rules

States (`gpu_lifecycle.rs:59`):

| State | Usable | Accepts new work | Meaning |
|---|---|---|---|
| `Ready { generation }` | yes, if stamp matches | yes | device healthy |
| `Lost { generation, reason }` | no | no | `DEVICE_REMOVED/RESET/HUNG/OTHER` |
| `Resetting { generation }` | no | no | between begin and recreated |
| `Recreated { generation }` | yes for the **new** generation; old stamps stay stale | yes | new device set installed |

Rules enforced by the pure state machine:

1. Generation only ever increases; `device_recreated(gen)` with `gen <= current`
   is refused and the state is left untouched (`:213`, tested at `:429`).
2. `begin_reset` on an already-resetting device is refused, not fatal (`:179`,
   tested at `:392`) — duplicate platform callbacks are expected.
3. `device_lost` while resetting keeps `Resetting` (`:194`, tested at `:418`).
4. `observe_generation` self-heals a missed reset-end: seeing the platform's
   newer generation moves the state forward (`:236`, tested at `:508`).
5. A resource stamped with generation G is usable only while the state is
   `Ready{G}` or `Recreated{G}` (`is_usable`, `:92`). `Recreated` deliberately
   keeps old stamps stale.
6. `should_rebuild_freeze(state, visual_generation)` returns true exactly once
   per generation when a rebuild can succeed (`:274`, tested at `:525`).
7. `acknowledge_recovery` returns to `Ready` only from `Recreated` (`:319`,
   tested at `:482`, `:494`).

Enforcement points: `Gpu::ensure_current` on every GPU op (`d3d.rs:174`),
`crop_sdr_bgra` (`capture/mod.rs:302`), `WgcLive::take_latest`
(`capture/wgc.rs:222`), `present_freeze_from_texture` (`gpui_bridge.rs:31`),
`capture_from_app` (`app.rs:113`), and per-tick long-capture checks
(`overlay/mod.rs:1620`). Failed D3D calls are classified by
`GetDeviceRemovedReason` and drive `Lost` (`gpu_lifecycle.rs:339`).

Recovery path: reset-end → `note_device_recreated` → next painted frame →
`maybe_recover_gpu` → `recover_gpu_after_reset` → re-capture with the new
generation, present a fresh freeze swap chain, re-attach and show
"截图窗口已恢复"; on failure the overlay closes with an empty result instead of
staying on an empty visual. If recoverable, `acknowledge_recovery` clears the
gate.

---

## 6. Long-capture quality rules

Thresholds (`longcapture.rs`):

| Rule | Value | Line |
|---|---|---|
| Static-band "the strip found itself" | `STATIC_EDGE_SCORE = 0.97` | `:98` |
| Independent-band minimum agreement before an offset is accepted | `VERIFY_MIN_SCORE = 0.85` | `:105` |
| Seam roughness allowed vs the frame's own row-to-row variation | `SEAM_MAX_RATIO = 1.9` | `:112` |
| Absolute floor for that baseline (flat pages) | `SEAM_BASELINE_FLOOR = 6.0` | `:117` |
| Rows averaged on each side of the seam | `SEAM_ROWS = 3` | `:121` |
| No-progress ticks before `Paused`/`Stalled` | `STALL_TICKS = 10` (1.5 s at 150 ms) | `:903` |
| Poll decimation while paused | every 4th tick (`SLOW_POLL_EVERY`) | `overlay/mod.rs:291` |

Rules:

1. An offset found by the strip SAD pass must reproduce a **band that took no
   part in the search** with score ≥ 0.85, or it is refused (`verify_shift`,
   `:409`). A rejected candidate is reported, never guessed.
2. The row pair straddling the seam must not be more than 1.9× rougher than the
   frame's interior row difference (floored at 6 luma levels). This catches a
   one-period slip on repeated content, which the strip score alone cannot.
3. A strip that matches itself at ≥ 0.97 with zero new rows is a `static_edge`:
   the page is not followed by guessing; the host shows
   "固定页眉/页脚遮挡，请缩小选区" immediately.
4. Unchanged frames are `Paused` (bar stays, cadence drops); changed frames with
   no confirmable offset for 10 ticks are `Stalled` ("未检测到滚动/内容未变化").
5. Fast movement beyond the strip is `FastScroll` ("滚动过快，请慢一点") and is
   not stitched.
6. All stitched rows still go through `Stitcher::append/prepend`, which are
   budget-checked and reject width mismatch / zero rows / height cap with
   distinct errors instead of one "max height" message.

Every tick writes `seam`, `verify`, `static_edge`, `changed`, `rejected_by_verify`
and `idle` to `diag.log` (`longworker.rs:552`, `:597`).

---

## 7. Test & verification results

Environment: Windows 11, debug test profile, `TEMP`/`TMP` set to
`D:/code/FastPin/target/test-temp` for the recorded runs.

### 7.1 `cargo fmt --check` — **FAIL**

```
EXIT=1
86 diff hunks in 22 files
```

| File | Hunks | Touched this round? |
|---|---|---|
| `crates/fastpin/src/longcapture.rs` | 18 | yes |
| `crates/fastpin/src/settings/mod.rs` | 13 | no |
| `crates/fastpin/src/platform/tray_menu.rs` | 7 | no |
| `crates/fastpin/src/image_budget.rs` | 6 | new |
| `crates/fastpin/src/settings/widgets.rs` | 5 | no |
| `crates/fastpin/src/pin.rs` | 5 | yes (small) |
| `crates/fastpin/src/gpu_lifecycle.rs` | 5 | new |
| `crates/fastpin/src/overlay/annotate.rs` | 4 | no |
| `crates/fastpin/src/brand_icon.rs` | 3 | no |
| `crates/fastpin/examples/gen_icon.rs` | 3 | no |
| `crates/fastpin/src/overlay/mod.rs` | 3 | yes |
| `crates/fastpin/src/platform/tray.rs` | 2 | no |
| `crates/fastpin/src/overlay/composite.rs` | 2 | no |
| `crates/fastpin/src/migrate.rs` | 2 | no |
| `crates/fastpin/src/translate.rs` | 1 | no |
| `crates/fastpin/src/settings/system.rs` | 1 | no |
| `crates/fastpin/src/settings/model.rs` | 1 | no |
| `crates/fastpin/src/overlay/toolbar.rs` | 1 | no |
| `crates/fastpin/src/ocr/mod.rs` | 1 | no |
| `crates/fastpin/src/longworker.rs` | 1 | new |
| `crates/fastpin/src/lib.rs` | 1 | yes |
| `crates/fastpin/src/app.rs` | 1 | yes |

40 hunks are in files this round touched (8 files, 3 of them new); 46 hunks in
14 files are pre-existing drift. The audit measured 52 hunks / 16 files at HEAD,
so Phase 1 added 34 hunks of its own drift. **Not fixed by this worker** (no
repo-wide format allowed).

### 7.2 `cargo clippy -p fastpin --all-targets` — 0 errors, 5 warnings

```
warning: this `impl` can be derived                      --> src\config.rs:153
warning: this function has too many arguments (9/7)      --> src\overlay\composite.rs:1034
warning: field assignment outside of initializer ...     --> src\config.rs:479 (478)
warning: field assignment outside of initializer ...     --> src\config.rs:583 (582)
warning: field assignment outside of initializer ...     --> src\config.rs:607 (606)
warning: `fastpin` (lib) generated 2 warnings
warning: `fastpin` (lib test) generated 5 warnings (2 duplicates)
Finished `dev` profile ... EXIT=0
```

All five are the same warnings the audit found at HEAD (same files/lines); Phase 1
introduced none. Locally this is green because `-D warnings` is not set.

**CI impact:** `.github/workflows/ci.yml:9` runs
`cargo clippy --workspace --all-targets --locked -- -D warnings`, so these five
promote to errors and **CI stays red** — independently of Phase 1. Exact lints:
`clippy::derivable_impls` (config.rs:153), `clippy::too_many_arguments`
(overlay/composite.rs:1034), `clippy::field_reassign_with_default` ×3
(config.rs:478/479, 582/583, 606/607).

### 7.3 `cargo test -p fastpin` — 319 passed / 1 failed, plus an intermittent crash

Total 320 tests (was 268 at the audit; +52 from Phase 1).

Default (parallel) runs, 12 attempts:

| Outcome | Count |
|---|---|
| Completed: `319 passed; 1 failed` | 6 |
| Aborted with `0xc0000005 STATUS_ACCESS_VIOLATION` after 312–313 `ok` | 6 |

Every completed run had exactly the same single failure:

```
failures:
    settings::tests::source_only_keeps_legacy_migration_literals

D:\code\FastPin\PHASE1_AUDIT_REPORT.md:653: ... README.md:29: ...
test result: FAILED. 319 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.42s
```

`-- --test-threads=1` (one run, as requested):

```
test result: FAILED. 319 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.30s
error: test failed, to rerun pass `-p fastpin --lib`
EXIT=101
```

No access violation in the single-threaded run. Two discriminators, recorded as
observations only (root cause **not** established):

| Experiment | Runs | Crashes |
|---|---|---|
| full suite, parallel (`--test-threads=16`, `--nocapture`) | 4 | 2 |
| full suite, parallel, `--skip longworker` | 10 | 1 |
| `longworker::tests` alone, parallel | 8 | 0 |
| `longcapture::tests::coarse*` alone, repeated | 6 | 0 |
| `brand_icon::tests::generated_brand*` alone, repeated | 6 | 0 |

The crash always lands in the last ~7 tests (the six that never print plus the
intentional failure), and the last `ok` line differs between runs, so it is not
one deterministic test. It is treated as an **open blocker**, not "fixed".

**Failure cause (unchanged, user-owned):** the legacy-name scan
(`settings/mod.rs:1944+`) exempts README lines containing the marker
`二次开发`, but `README.md:29` says `二次vibe coding开发`, so the marker does not
match. `PHASE1_AUDIT_REPORT.md:653` quotes that line, so the audit file also
trips the scan. Fixing this requires a user decision (see §8).

### 7.4 `cargo build --release --locked` — **PASS**

```
    Finished `release` profile [optimized] target(s) in 0.55s
EXIT=0
target/release/FastPin.exe  36,678,144 bytes
```

(The build was already up to date with the working tree; the binary is 110,592 B
larger than the HEAD build the audit measured, consistent with the added code.)

### 7.5 Test delta

| Module | Tests at HEAD | Now | Added |
|---|---|---|---|
| `image_budget` | 0 | 12 | +12 |
| `gpu_lifecycle` | 0 | 15 | +15 |
| `longworker` | 0 | 7 | +7 |
| `longcapture` | 28 | 45 | +17 |
| `codec` | 2 | 3 | +1 |
| everything else | 238 | 238 | 0 |
| **total** | **268** | **320** | **+52** |

Result: 319 pass, 1 fail (the README-scan test), in both single-threaded and
completed parallel runs.

### 7.6 What is *not* measured

Release-profile tick latency, grayscale/SAD/stitch throughput, WGC copy and
staging `Map` cost, freeze present cost, memory high-water mark, TDR recovery
time, and any UI responsiveness figure: **not measured (pending Windows
runtime)**. The only numbers in this report are the two harness outputs quoted in
§2 (span line, prepend copy count) and the command timings above.

---

## 8. Status matrix

| Area | Automated tests verified | Windows runtime verified | Manual acceptance pending | Known limitations |
|---|---|---|---|---|
| P1-01 tick split / worker | yes (7 tests, span line) | no | tick latency under real WGC, overlay responsiveness | fallback capture still starts a session per tick; diag write still synchronous inside the worker (P2-02) |
| P1-02 headroom prepend | yes (pixel-exact + copy-count tests) | no | 2000 px / 8K upward scroll memory profile | headroom re-grant still copies the image (by design, geometric) |
| P1-03 image budget | yes (12 + 3 + 1 tests) | no | 16k pin / large group export, clipboard at 16k | ocr, watermark, pin render image, annotation backdrops not routed |
| P1-04 GPU reset | yes (15 state-machine tests) | no | TDR idle / overlay / long capture / device removal (matrix rows 9-12) | no hardware TDR executed; WGC session is terminated, not restarted |
| P1-05 quality rules | yes (12 longcapture + worker verdict tests) | no | sticky header/footer on a real page, repeated table, 1-2 px, fast scroll (rows 16-19, 21-22) | no alternate strip position is tried — the page is reported, not followed |
| Legacy-name scan | fails deterministically | n/a | user decision required | `README.md:29` + audit quote |
| Formatting | fails (86 hunks) | n/a | none | out of scope this round |
| Clippy `-D warnings` | fails in CI (5 warnings) | n/a | none | pre-existing, not introduced by Phase 1 |
| Parallel test crash | intermittent (6/12) | n/a | none | root cause unknown; single-threaded clean |

---

## 9. Completion criteria checklist

| # | Criterion | Status |
|---|---|---|
| 1 | No P0 defect open | **Met** — audit found none; none introduced |
| 2 | P1-01..P1-05 implemented in the working tree | **Met** |
| 3 | A regression test exists for each P1 item | **Met** — 52 new tests; P1-04 decision logic only (hardware TDR not testable) |
| 4 | `cargo build --release --locked` green | **Met** |
| 5 | `cargo clippy -p fastpin --all-targets` no errors | **Met** locally; **blocked** for CI (`-D warnings`, 5 pre-existing lints) |
| 6 | `cargo test -p fastpin` green | **Blocked** — 1 deterministic failure (README scan) + intermittent 0xc0000005 in parallel runs |
| 7 | `cargo fmt --check` green | **Blocked** — 86 hunks in 22 files; no formatting allowed this round |
| 8 | `README.md` and CI untouched | **Met** |
| 9 | Nothing committed or pushed | **Met** — HEAD `4b039b7` |
| 10 | Runtime acceptance on Windows hardware | **Blocked** — TDR, HDR, multi-monitor, 16k export, clipboard interop not executed in this environment |
| 11 | P1-03 routed through every listed entry point | **Partially met** — see residue below |

P1-03 residue, explicitly **not** fixed (do not read the P1-03 "Fixed" label as
covering these): `ocr::to_rgb` (`ocr/mod.rs:237`), `composite::load_watermark_image`
(`overlay/composite.rs:1122`), `pin::render_image` duplicate buffer (P2-05), and
`overlay/composite.rs:588/616/724/789` annotation buffers.

The three CI-facing blockers are **undecided, not fixed**:

1. **Formatting:** 86 hunks / 22 files. Needs a scoped `cargo fmt` decision
   (whole crate vs touched files only) — not taken here.
2. **README scan:** needs a user decision between restoring the exact marker
   text in `README.md` and changing the test's exemption rule. Editing either
   without that decision would violate the standing instruction.
3. **Clippy `-D warnings`:** the 5 lints are pre-existing at HEAD; Phase 1 added
   none. CI will stay red until they are cleaned or the workflow policy changes.

---

## 10. Remaining risks (P2/P3 summary)

Unchanged and still open (see `PHASE1_DEFECTS.md`, P2/P3 keep `Open`):

- **P2-01** live-session start still blocks the UI up to 500 ms (the long capture
  start path was not moved off-thread).
- **P2-02** `diag.log` is still opened/flushed per tick — now on the worker, so it
  no longer stalls the UI, but it is still synchronous I/O.
- **P2-03** a short tile is still skipped in `compose` (now with a precise error
  for the *canvas*, but a short tile buffer still continues silently).
- **P2-04** pin-edit compositing still runs on the UI thread and keeps a full
  backdrop.
- **P2-05** every pin still stores the image twice; `render_image` is not routed
  through the budget.
- **P2-06** translation response body still has no size cap.
- **P2-07** OCR engine lock is still held across inference; one idle timer thread
  per call.
- **P2-08** partially mitigated: the DIB size math is now checked (P1-03), but a
  16k image still needs a full DIB copy; no PNG-clipboard fallback.
- **P2-09** watermark decode has no size cap.
- **P2-10** CI red / fmt not gated — partially addressed: the state is now
  measured and documented, nothing was changed.
- **P2-11** the quality matrix gap is closed by the P1-05 tests for synthetic
  sticky/periodic/fast/1-2 px/seam cases; real pages remain manual.
- **P2-12** capture errors are still swallowed per tick; the stall tracker covers
  "changed but unalignable", not a persistently failing capture.
- **P3-01** partially improved: rejection reasons are distinct errors now, but
  the status line still maps several of them to one string.
- **P3-07** closed by `ResultGate` (generation/epoch/seq re-checked before
  applying a result).
- **P3-02** closed for `set_clipboard_dib` (checked sizes).
- **P3-03/P3-04/P3-05/P3-06/P3-08** untouched.

Operational risk: the intermittent parallel-test access violation means a CI run
can fail non-deterministically. On a clean checkout this is masked by the
deterministic README failure, so it must be tracked separately, not assumed fixed
by a later green run.

---

## 11. Uncommitted-work inventory

`HEAD` is `4b039b7`; nothing below is staged or committed. `README.md`, `LICENSE`
and `.github/` do not appear in any of these lists.

Modified (17 files, +2370 / −330):

| File | Δ | Phase 1 item |
|---|---|---|
| `crates/fastpin/src/longcapture.rs` | 1601 | P1-02, P1-03, P1-05 |
| `crates/fastpin/src/overlay/mod.rs` | 588 | P1-01, P1-03, P1-04, P1-05 |
| `crates/fastpin/src/capture/d3d.rs` | 100 | P1-04 |
| `crates/fastpin/src/pin_snap.rs` | 94 | P1-03 |
| `crates/fastpin/src/gpui_bridge.rs` | 73 | P1-04 |
| `crates/fastpin/src/codec/mod.rs` | 38 | P1-03 |
| `patches/gpui-pre-windows-0.3.9-fastpin-overlay.patch` | 61 | P1-04 |
| `patches/gpui-pre-0.3.9-fastpin-overlay.patch` | 34 | P1-04 |
| `third_party/gpui-pre-windows/src/platform.rs` | 23 | P1-04 |
| `crates/fastpin/src/capture/wgc.rs` | 18 | P1-04 |
| `crates/fastpin/src/pin_edit.rs` | 18 | P1-03 |
| `crates/fastpin/src/app.rs` | 15 | P1-04 |
| `crates/fastpin/src/capture/mod.rs` | 11 | P1-04 |
| `crates/fastpin/src/pin.rs` | 11 | P1-03 |
| `third_party/gpui-pre/src/app.rs` | 7 | P1-04 |
| `third_party/gpui-pre/src/platform.rs` | 5 | P1-04 |
| `crates/fastpin/src/lib.rs` | 3 | P1-01, P1-03, P1-04 |

Untracked to add with the commit:

| Path | Lines | Purpose |
|---|---|---|
| `crates/fastpin/src/image_budget.rs` | 513 | P1-03 |
| `crates/fastpin/src/longworker.rs` | 913 | P1-01 |
| `crates/fastpin/src/gpu_lifecycle.rs` | 589 | P1-04 |
| `PHASE1_AUDIT_REPORT.md` | 810 | Phase 1 audit (previous round) |
| `PHASE1_DEFECTS.md` | 355 | Phase 1 defect list (this round updated with statuses) |
| `PHASE1_IMPLEMENTATION_REPORT.md` | this file | implementation report |

Deliberately not part of the change set: `README.md` (contains the legacy-name
line the failing test is about), `.github/` (CI), `LICENSE`, and the untracked
`.npmcache/` directory (tooling cache, should not be added).

Suggested review order before committing: P1-03 constants and `compose` →
P1-02 stitcher headroom → P1-01 worker + overlay tick → P1-05 thresholds →
P1-04 lifecycle and the regenerated patches.
