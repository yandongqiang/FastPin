//! GPUI overlay bridge: freeze composition swapchain (visual 0) under GPUI UI.

use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

use crate::capture::RestoreGpu;
use crate::error::{Error, Result};

/// Cached freeze swap chain plus the device generation it was created on.
static FREEZE_SWAP: Mutex<Option<(IDXGISwapChain1, u32, u32, u64)>> = Mutex::new(None);

/// Drop the cached freeze swapchain (GPU reset).
pub fn drop_freeze_swapchain() {
    *FREEZE_SWAP.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// Create or resize a composition swapchain, copy `src` into it, and Present.
///
/// `generation` is the device generation the source texture belongs to; a
/// cached chain from another generation is discarded instead of reused.
pub fn present_freeze_from_texture(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    width: u32,
    height: u32,
    generation: u64,
) -> Result<IDXGISwapChain1> {
    if !crate::gpu_lifecycle::is_usable(generation) {
        return Err(Error::new(format!(
            "stale freeze frame: generation {generation} is no longer usable"
        )));
    }
    let _restore = RestoreGpu(context);
    let width = width.max(1);
    let height = height.max(1);
    let swap = reuse_or_create_swapchain(device, width, height, generation)?;
    unsafe {
        let buf: ID3D11Texture2D = match swap.GetBuffer(0) {
            Ok(buf) => buf,
            Err(e) => {
                crate::gpu_lifecycle::note_failed_device_operation(device);
                return Err(e.into());
            }
        };
        context.CopyResource(&buf, src);
        if let Err(e) = swap.Present(0, DXGI_PRESENT(0)).ok() {
            crate::gpu_lifecycle::note_failed_device_operation(device);
            return Err(e.into());
        }
    }
    Ok(swap)
}

fn reuse_or_create_swapchain(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    generation: u64,
) -> Result<IDXGISwapChain1> {
    let mut slot = FREEZE_SWAP.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((swap, w, h, gen)) = slot.as_ref() {
        if *gen == generation {
            if *w == width && *h == height {
                return Ok(swap.clone());
            }
            unsafe {
                if swap
                    .ResizeBuffers(
                        2,
                        width,
                        height,
                        DXGI_FORMAT_B8G8R8A8_UNORM,
                        DXGI_SWAP_CHAIN_FLAG(0),
                    )
                    .is_ok()
                {
                    let swap = swap.clone();
                    *slot = Some((swap.clone(), width, height, generation));
                    return Ok(swap);
                }
            }
        }
    }
    // A chain from an older device cannot be resized onto the new one, and a
    // failed resize leaves it unusable; let it drop and create a fresh chain.
    *slot = None;
    let swap = unsafe { create_composition_swapchain(device, width, height)? };
    *slot = Some((swap.clone(), width, height, generation));
    Ok(swap)
}

unsafe fn create_composition_swapchain(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<IDXGISwapChain1> {
    unsafe {
        let dxgi_device: IDXGIDevice = device.cast()?;
        let adapter = dxgi_device.GetAdapter()?;
        let factory: IDXGIFactory2 = adapter.GetParent()?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            Flags: 0,
        };
        Ok(factory.CreateSwapChainForComposition(device, &desc, None)?)
    }
}
