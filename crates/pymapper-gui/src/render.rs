//! One frame of GPU work: clear, canvas, then egui on top, in a single render pass into any
//! texture view (the swapchain image, or an offscreen texture in tests).

use crate::canvas::{Canvas, PhysicalRect};

/// Everything a frame needs from the UI pass.
pub struct FrameInput<'a> {
    pub primitives: &'a [egui::ClippedPrimitive],
    /// Texture uploads/frees for this frame; drained by [`Renderer::render`].
    pub textures: &'a mut egui::TexturesDelta,
    /// Target size in physical pixels.
    pub size: [u32; 2],
    pub pixels_per_point: f32,
    /// Canvas area in physical pixels; `None` when panels cover the whole window.
    pub canvas: Option<PhysicalRect>,
}

pub struct Renderer {
    egui: egui_wgpu::Renderer,
    canvas: Canvas,
    format: wgpu::TextureFormat,
}

impl Renderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        Self {
            // No MSAA (egui feathers its own edges), no depth buffer.
            egui: egui_wgpu::Renderer::new(device, format, egui_wgpu::RendererOptions::default()),
            canvas: Canvas::new(device, format),
            format,
        }
    }

    pub const fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// Records and submits the frame into `target` (which must be `input.size` and of
    /// [`Self::format`]). Drains `input.textures`.
    pub fn render(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, target: &wgpu::TextureView, input: FrameInput<'_>) {
        for (id, deltas) in input.textures.set.drain() {
            for delta in &deltas {
                self.egui.update_texture(device, queue, id, delta);
            }
        }
        let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: input.size, pixels_per_point: input.pixels_per_point };
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        let callbacks = self.egui.update_buffers(device, queue, &mut encoder, input.primitives, &screen);
        if let Some(rect) = input.canvas {
            self.canvas.prepare(queue, rect, input.pixels_per_point);
        }
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("frame"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(self.canvas.clear_color()),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                // egui's renderer takes a 'static pass; the encoder is not touched until the
                // pass is dropped at the end of this block.
                .forget_lifetime();
            if let Some(rect) = input.canvas {
                self.canvas.draw(&mut pass, rect);
            }
            // egui sets its own scissor rect per primitive.
            self.egui.render(&mut pass, input.primitives, &screen);
        }
        for id in input.textures.free.drain() {
            self.egui.free_texture(&id);
        }
        queue.submit(callbacks.into_iter().chain(std::iter::once(encoder.finish())));
    }
}
