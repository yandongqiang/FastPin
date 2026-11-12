# gpui-pre upstream

- crate: gpui-pre
- version: 0.3.9
- registry: crates.io
- license: Apache-2.0
- checksum: 34f61c1cfd36fc9c436af09308c963822ae27a45a84feb21a77ba5c9e051f1c0
- consumed by: gpui-kit 0.8.0 (renamed dependency `gpui = "=0.3.9"`)
- local patches:
  1. Renamed `windows61` dependency so patched-in public API signatures use the app's `windows` 0.61 types
  2. `Platform` trait + `App`: gpu_generation / adapter_luid / with_gpu_context / on_gpu_reset_begin / release_gpu_devices
  3. `PlatformWindow` trait + `Window`: set_background_swap_chain / wait_for_commit_completion / on_app_ready_sentinel

See `patches/README.md` for details and `patches/gpui-pre-0.3.9-fastpin-overlay.patch`
for the exact delta.
