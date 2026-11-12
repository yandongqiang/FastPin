# gpui-pre-windows upstream

- crate: gpui-pre-windows
- version: 0.3.9
- registry: crates.io
- license: Apache-2.0
- checksum: 513d4f0fc7d596d3bdef2a73a524ffb83dabc95bf484f2855a698e7f431290a9
- local patches:
  1. DirectComposition dual visuals (freeze visual under the GPUI UI visual) + set_freeze_swap_chain + wait_for_commit_completion
  2. DirectXDevices adapter_luid / is_available
  3. WindowsPlatform: gpu_generation (bumped on the main thread next to the device swap), WM_GPUI_GPU_RESET_BEGIN sent from the vsync thread before device recreation, with_gpu_context borrowing the device across the windows 0.61/0.62 boundary via as_raw + ManuallyDrop::from_raw, release_gpu_devices -> detach_gpu
  4. Window: app_ready_sentinel callback (WM_APP + 32 / 0x8020) and the three patched PlatformWindow methods

See `patches/README.md` for details and
`patches/gpui-pre-windows-0.3.9-fastpin-overlay.patch` for the exact delta.
