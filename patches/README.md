# Vendored gpui-pre deltas

`third_party/gpui-pre` and `third_party/gpui-pre-windows` are **gpui-pre 0.3.9**
and **gpui-pre-windows 0.3.9** from crates.io, each with a local patch, wired in
through `[patch.crates-io]` in the workspace `Cargo.toml`. The FastPin application
itself depends on `gpui-kit = "=0.8.0"` (which renames `gpui-pre` as its `gpui`
dependency), so the patched crates are what the app actually compiles against.

The old `third_party/gpui` (gpui 0.2.2 + its patch, `gpui-0.2.2-fastpin-overlay.patch`)
is kept untouched for reference only; nothing consumes it after the gpui-kit
migration.

## What the patches add

`gpui-pre-0.3.9-fastpin-overlay.patch` (155 added, 4 removed) — the platform-neutral
surface:

| File | Change |
| --- | --- |
| `Cargo.toml` | Renamed `windows61` dependency (`windows 0.61`) so the patched public API signatures use the same `windows` major that applications compile against. Type identity comes from the crate version, so `windows61::ID3D11Device` **is** the app's `windows::ID3D11Device`. |
| `src/platform.rs` | `Platform` trait: `gpu_generation`, `adapter_luid`, `with_gpu_context`, `on_gpu_reset_begin` (windows-only, default no-op) and `release_gpu_devices`. `PlatformWindow` trait: `set_background_swap_chain`, `wait_for_commit_completion`, `on_app_ready_sentinel` (windows-only, default no-op). |
| `src/app.rs` | `App::gpu_generation / adapter_luid / with_gpu_context / on_gpu_reset_begin / release_gpu_devices`. |
| `src/window.rs` | `Window::set_background_swap_chain / wait_for_commit_completion / on_app_ready_sentinel`. |

`gpui-pre-windows-0.3.9-fastpin-overlay.patch` (256 added, 13 removed) — the
Windows implementation:

| File | Change |
| --- | --- |
| `Cargo.toml` | Same renamed `windows61` dependency. |
| `src/directx_devices.rs` | `DirectXDevices::adapter_luid()`, `is_available()`. |
| `src/directx_renderer.rs` | **DirectComposition dual visuals**: `root_visual` contains a `freeze_visual` (below) and a `ui_visual` (above, inserted with an explicit reference to the freeze visual); `set_freeze_swap_chain(Option<&IDXGISwapChain1>)`, `wait_for_commit_completion`. Renderer-level `set_background_swap_chain`. |
| `src/platform.rs` | `WindowsPlatformInner.gpu_generation` (bumped on the main thread next to each device swap), `PlatformCallbacks.gpu_reset_begin`, `WM_GPUI_GPU_RESET_BEGIN` handling, the vsync thread sends reset-begin before device recreation, `with_gpu_context` (borrows the COM pointers across the 0.61/0.62 boundary via `as_raw` + `ManuallyDrop::from_raw` — no reference-count changes), `release_gpu_devices` → `detach_gpu`. |
| `src/events.rs` | `WM_GPUI_GPU_RESET_BEGIN` (`WM_USER + 9`) and the `WM_APP + 32` (0x8020) overlay-ready sentinel handling. |
| `src/window.rs` | `Callbacks.app_ready_sentinel`, `set_background_swap_chain` (converts the 0.61 swap chain to the renderer's 0.62 type), `wait_for_commit_completion`, `on_app_ready_sentinel`. |

`AddVisual` z-order in `directx_renderer.rs` is load-bearing and easy to get
backwards: with `referenceVisual = NULL` the meaning of `insertAbove` inverts, so
the UI visual is added with an explicit reference to the freeze visual instead.
Getting this wrong puts the opaque freeze frame on top and hides every piece of
overlay UI while every automated gate still passes.

FastPin depends on all of it: the freeze frame is DComp visual 0 under GPUI's own
UI visual, capture shares GPUI's `ID3D11Device` (borrowed through
`App::with_gpu_context`), and the T7 perf mark is the `WM_APP+32` sentinel.
`crate::platform::WM_APP_READY_SENTINEL` must stay equal to the value in
`events.rs`.

Two deliberate omissions relative to the 0.2.2 patch:

- `WindowOptions.topmost` no longer exists; gpui-pre creates every
  `WindowKind::PopUp` with `WS_EX_TOPMOST` unconditionally, which covers all
  FastPin popup windows (overlay, pins, pin edit toolbar).
- `App::set_quit_on_last_window_close(false)` is replaced by upstream's
  `App::set_quit_mode(QuitMode::Explicit)`.
- `Window::win32_hwnd` was not re-added; FastPin now derives the HWND itself via
  `crate::platform::hwnd_of` (`HasWindowHandle` → `Win32WindowHandle.hwnd`).

`App::release_gpu_devices` is currently unused by FastPin; it exists because the
plan requires an app-side release path, and maps to `detach_gpu` (vsync stop +
device drop).

## Regenerating the patches

Run from the repo root after changing anything under `third_party/gpui-pre*`:

```bash
bash patches/regen.sh
```

The pristine trees must be in the cargo registry cache (they are: FastPin pulls
the same versions).
