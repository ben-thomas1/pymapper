//! GPU setup: one wgpu instance/adapter/device/queue shared by the canvas and egui.
//!
//! Backends: Metal on macOS, Vulkan on Linux (Wayland or X11 surfaces; winit picks Wayland
//! when `WAYLAND_DISPLAY` is set). Other targets fall back to wgpu's primary backends and are
//! unsupported. `WGPU_BACKEND` overrides the choice for debugging.

use anyhow::{Context, Result, anyhow};

/// Backends compiled in and requested for this target.
pub const BACKENDS: wgpu::Backends = if cfg!(target_os = "macos") {
    wgpu::Backends::METAL
} else if cfg!(target_os = "linux") {
    wgpu::Backends::VULKAN
} else {
    wgpu::Backends::PRIMARY
};

/// Frames the presentation engine may queue ahead. 2 keeps input latency low while letting
/// the CPU prepare the next frame during scan-out.
pub const FRAME_LATENCY: u32 = 2;

pub fn instance() -> wgpu::Instance {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::from_env().unwrap_or(BACKENDS);
    // Validation in debug builds, none in release; env vars (WGPU_DEBUG, ...) may override.
    desc.flags = wgpu::InstanceFlags::from_build_config().with_env();
    wgpu::Instance::new(desc)
}

/// Picks an adapter able to present to `surface` (if given). Prefers the low-power GPU: the
/// code map is 2D and battery matters more than peak fill rate.
pub fn adapter(instance: &wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> Result<wgpu::Adapter> {
    let options = wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::LowPower),
        force_fallback_adapter: false,
        compatible_surface: surface,
        ..Default::default()
    };
    pollster::block_on(instance.request_adapter(&options))
        .map_err(|e| anyhow!("no suitable GPU adapter for backends {:?}: {e}", BACKENDS))
}

pub fn device(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue)> {
    let desc = wgpu::DeviceDescriptor {
        label: Some("pymapper"),
        required_features: wgpu::Features::empty(),
        // Default limits, but allow textures as large as the adapter supports (big HiDPI
        // displays, glyph atlases).
        required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
        // Memory is a first-class budget: prefer small allocations over speed of allocation.
        memory_hints: wgpu::MemoryHints::MemoryUsage,
        ..Default::default()
    };
    pollster::block_on(adapter.request_device(&desc)).context("requesting GPU device")
}

/// One-line adapter description shown in the UI.
pub fn describe(adapter: &wgpu::Adapter) -> String {
    let info = adapter.get_info();
    format!("{} ({:?}, {:?})\ndriver: {} {}", info.name, info.backend, info.device_type, info.driver, info.driver_info)
}

/// Surface extent for a window of `size` physical pixels: `None` when minimised (zero area),
/// otherwise clamped to the device's maximum texture dimension.
pub fn surface_extent(width: u32, height: u32, max_dim: u32) -> Option<(u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    Some((width.min(max_dim), height.min(max_dim)))
}

/// Surface configuration: a gamma-space (non-sRGB) format when available, as egui prefers;
/// vsync'd FIFO presentation (on-demand rendering means vsync costs nothing when idle).
pub fn surface_config(
    surface: &wgpu::Surface<'_>,
    adapter: &wgpu::Adapter,
    width: u32,
    height: u32,
) -> Result<wgpu::SurfaceConfiguration> {
    let caps = surface.get_capabilities(adapter);
    let format = egui_wgpu::preferred_framebuffer_format(&caps.formats).context("surface has no formats")?;
    let base = surface
        .get_default_config(adapter, width, height)
        .ok_or_else(|| anyhow!("surface is not supported by the adapter"))?;
    Ok(wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        present_mode: wgpu::PresentMode::AutoVsync,
        desired_maximum_frame_latency: FRAME_LATENCY,
        alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
        view_formats: Vec::new(),
        ..base
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_per_os() {
        #[cfg(target_os = "macos")]
        assert_eq!(BACKENDS, wgpu::Backends::METAL);
        #[cfg(target_os = "linux")]
        assert_eq!(BACKENDS, wgpu::Backends::VULKAN);
    }

    #[test]
    fn extent_rejects_zero_and_clamps() {
        assert_eq!(surface_extent(0, 100, 8192), None);
        assert_eq!(surface_extent(100, 0, 8192), None);
        assert_eq!(surface_extent(1, 1, 8192), Some((1, 1)));
        assert_eq!(surface_extent(3840, 2160, 8192), Some((3840, 2160)));
        assert_eq!(surface_extent(20000, 100, 16384), Some((16384, 100)));
    }
}
