//! Compositing gex's Wayland surfaces on the GPU.
//!
//! The compositor in [`gex_wayland`] produces [`Frame`]s of client pixels;
//! this puts them on a screen without touching every pixel on the CPU. Built
//! on wgpu (the WebGPU API): native it targets Vulkan or GL, `wasm32` it
//! targets the browser's WebGPU, so the compositor is written once.
//!
//! It does not give the *guest* a GPU: a client still draws its own pixels
//! into shared memory and this uploads them. It removes the compositor's half
//! of the cost, not the client's.

use gex_wayland::Frame;
use std::collections::HashMap;

/// Why a GPU backend could not be started. Each is a reason to fall back to
/// CPU compositing rather than to fail.
#[derive(Debug)]
pub enum GpuError {
    /// Nothing on this machine implements WebGPU, Vulkan or GL.
    NoAdapter,
    /// An adapter exists but would not give us a device.
    NoDevice(String),
    /// A draw or a read-back failed.
    Lost(String),
}

impl std::fmt::Display for GpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpuError::NoAdapter => f.write_str("no graphics adapter"),
            GpuError::NoDevice(e) => write!(f, "no device: {e}"),
            GpuError::Lost(e) => write!(f, "device lost: {e}"),
        }
    }
}

impl std::error::Error for GpuError {}

/// What the shader needs to know about one surface. Sixteen-byte aligned,
/// because that is what a uniform buffer requires.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct SurfaceUniform {
    rect: [f32; 4],
    screen: [f32; 2],
    opaque: f32,
    _pad: f32,
}

/// One client buffer, kept as a texture between frames and keyed by surface so
/// a redraw reuses it; recreated only when the size changes.
struct Cached {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

/// A GPU compositor drawing into an offscreen target.
pub struct Compositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    target: wgpu::Texture,
    target_view: wgpu::TextureView,
    /// Where read-back lands. Sized once, because it is as large as the
    /// screen and reallocating it every frame is a stall.
    readback: wgpu::Buffer,
    surfaces: HashMap<u32, Cached>,
    width: u32,
    height: u32,
    /// What the adapter turned out to be, for anyone asking whether this is
    /// real hardware or a software implementation of it.
    pub adapter: String,
}

/// Read-back rows must start on a 256-byte boundary.
const ROW_ALIGN: u32 = 256;

fn padded_row(width: u32) -> u32 {
    let unpadded = width * 4;
    unpadded.div_ceil(ROW_ALIGN) * ROW_ALIGN
}

impl Compositor {
    /// Start a compositor for a screen of this size. Blocks while the adapter
    /// and device are requested (futures in WebGPU, but immediate in practice).
    #[cfg(not(target_family = "wasm"))]
    pub fn new(width: u32, height: u32) -> Result<Compositor, GpuError> {
        pollster::block_on(Compositor::new_async(width, height))
    }

    pub async fn new_async(width: u32, height: u32) -> Result<Compositor, GpuError> {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                // No surface: this composites into a texture, and what the
                // host then does with it -- a window, a canvas, a file -- is
                // not this crate's business.
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .map_err(|_| GpuError::NoAdapter)?;
        let info = adapter.get_info();
        let name = format!("{} ({:?}, {:?})", info.name, info.device_type, info.backend);

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("gex compositor"),
                // The defaults are the WebGPU baseline, which is the point:
                // asking for more here would work natively and fail in a
                // browser.
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_webgl2_defaults()
                    .using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::Performance,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|e| GpuError::NoDevice(e.to_string()))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("surface"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("composite"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("composite"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    // Alpha blend, so a surface shows what is under it where
                    // it is transparent.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("surface"),
            // Nearest, not linear: a window drawn at its own size must come
            // out pixel for pixel, and a test that compares pixels wants the
            // ones the client wrote.
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("screen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&Default::default());

        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded_row(width) * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        Ok(Compositor {
            device,
            queue,
            pipeline,
            layout,
            sampler,
            target,
            target_view,
            readback,
            surfaces: HashMap::new(),
            width,
            height,
            adapter: name,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Upload a frame's pixels into the surface's texture, making one if the
    /// size changed. Rows arrive with the client's (possibly padded) stride,
    /// which `write_texture` takes directly rather than stripping here.
    fn upload(&mut self, frame: &Frame) -> Result<(), GpuError> {
        let width = frame.width.max(0) as u32;
        let height = frame.height.max(0) as u32;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let stride = frame.stride.max(0) as u32;
        let entry = self.surfaces.entry(frame.surface);
        let cached = match entry {
            std::collections::hash_map::Entry::Occupied(e)
                if e.get().width == width && e.get().height == height =>
            {
                e.into_mut()
            }
            other => {
                let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("client buffer"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let view = texture.create_view(&Default::default());
                let fresh = Cached {
                    texture,
                    view,
                    width,
                    height,
                };
                match other {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        e.insert(fresh);
                        e.into_mut()
                    }
                    std::collections::hash_map::Entry::Vacant(e) => e.insert(fresh),
                }
            }
        };
        let need = (stride * height) as usize;
        if frame.pixels.len() < need {
            return Err(GpuError::Lost("frame is shorter than its stride".into()));
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &cached.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.pixels[..need],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        Ok(())
    }

    /// Draw the given surfaces, in order, onto a cleared screen. With no window
    /// manager, each surface is centred (larger-than-screen ones pinned to the
    /// corner) and a later one draws over an earlier one.
    pub fn draw(&mut self, frames: &[Frame]) -> Result<(), GpuError> {
        for frame in frames {
            self.upload(frame)?;
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("composite"),
            });
        // Bind groups have to outlive the render pass that uses them.
        let mut groups = Vec::new();
        for frame in frames {
            let Some(cached) = self.surfaces.get(&frame.surface) else {
                continue;
            };
            let x = ((self.width as f32 - cached.width as f32) / 2.0).max(0.0).floor();
            let y = ((self.height as f32 - cached.height as f32) / 2.0).max(0.0).floor();
            let uniform = SurfaceUniform {
                rect: [x, y, cached.width as f32, cached.height as f32],
                screen: [self.width as f32, self.height as f32],
                // Format 1 is XRGB8888: the top byte is padding, not alpha.
                opaque: if frame.format == 1 { 1.0 } else { 0.0 },
                _pad: 0.0,
            };
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (&uniform as *const SurfaceUniform) as *const u8,
                    std::mem::size_of::<SurfaceUniform>(),
                )
            };
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("surface"),
                size: std::mem::size_of::<SurfaceUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue.write_buffer(&buffer, 0, bytes);
            let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("surface"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&cached.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            groups.push((group, buffer));
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // An opaque black screen behind everything, so a
                        // window smaller than the output has something
                        // definite around it rather than whatever was there.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            for (group, _) in &groups {
                pass.set_bind_group(0, group, &[]);
                pass.draw(0..6, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        Ok(())
    }

    /// Copy the screen back to the CPU as tightly packed RGBA. Only for a host
    /// with nowhere to present to (a test, or a file); one with a window draws
    /// into its swapchain instead.
    #[cfg(not(target_family = "wasm"))]
    pub fn read_back(&self) -> Result<Vec<u8>, GpuError> {
        pollster::block_on(self.read_back_async())
    }

    pub async fn read_back_async(&self) -> Result<Vec<u8>, GpuError> {
        let row = padded_row(self.width);
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);

        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        // The map completes when the queue does, and nothing tells us the
        // queue is done except polling it.
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .map_err(|e| GpuError::Lost(format!("{e:?}")))?;
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(GpuError::Lost(e.to_string())),
            Err(e) => return Err(GpuError::Lost(e.to_string())),
        }
        let padded = slice.get_mapped_range();
        let mut out = Vec::with_capacity((self.width * self.height * 4) as usize);
        for y in 0..self.height {
            let start = (y * row) as usize;
            out.extend_from_slice(&padded[start..start + (self.width * 4) as usize]);
        }
        drop(padded);
        self.readback.unmap();
        Ok(out)
    }
}

/// Turn a composited screen into the RGB a PPM wants.
pub fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        out.extend_from_slice(&px[..3]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid-colour XRGB8888 frame, the way a client would hand one over:
    /// blue first in memory.
    fn solid(surface: u32, w: i32, h: i32, b: u8, g: u8, r: u8) -> Frame {
        let mut pixels = Vec::new();
        for _ in 0..w * h {
            pixels.extend_from_slice(&[b, g, r, 0xff]);
        }
        Frame {
            surface,
            width: w,
            height: h,
            stride: w * 4,
            format: 1,
            pixels,
            title: String::new(),
        }
    }

    fn at(rgba: &[u8], width: u32, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let o = ((y * width + x) * 4) as usize;
        (rgba[o], rgba[o + 1], rgba[o + 2], rgba[o + 3])
    }

    /// Every test needs a device, and a machine without one is not a failure.
    fn compositor(w: u32, h: u32) -> Option<Compositor> {
        match Compositor::new(w, h) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("skipping: {e}");
                None
            }
        }
    }

    #[test]
    fn a_surface_is_centred_and_its_colours_survive_the_swizzle() {
        let Some(mut c) = compositor(64, 64) else { return };
        // Blue byte 0x10, green 0x20, red 0x30, so the pixel is rgb(30,20,10).
        c.draw(&[solid(1, 16, 16, 0x10, 0x20, 0x30)]).unwrap();
        let out = c.read_back().unwrap();
        assert_eq!(out.len(), 64 * 64 * 4);
        // Centred: a 16-wide surface on a 64-wide screen starts at 24.
        assert_eq!(at(&out, 64, 32, 32), (0x30, 0x20, 0x10, 0xff), "centre");
        assert_eq!(at(&out, 64, 24, 24), (0x30, 0x20, 0x10, 0xff), "top-left");
        assert_eq!(at(&out, 64, 39, 39), (0x30, 0x20, 0x10, 0xff), "bottom-right");
        // And nothing outside it.
        assert_eq!(at(&out, 64, 23, 24), (0, 0, 0, 0xff), "just left of it");
        assert_eq!(at(&out, 64, 40, 39), (0, 0, 0, 0xff), "just right of it");
        assert_eq!(at(&out, 64, 0, 0), (0, 0, 0, 0xff), "the corner is clear");
    }

    #[test]
    fn a_later_surface_is_drawn_over_an_earlier_one() {
        let Some(mut c) = compositor(32, 32) else { return };
        c.draw(&[
            solid(1, 32, 32, 0, 0, 0xff),
            solid(2, 8, 8, 0, 0xff, 0),
        ])
        .unwrap();
        let out = c.read_back().unwrap();
        assert_eq!(at(&out, 32, 16, 16).1, 0xff, "the small green one is on top");
        assert_eq!(at(&out, 32, 0, 0).0, 0xff, "the big red one is behind it");
    }

    #[test]
    fn a_transparent_surface_lets_what_is_under_it_through() {
        let Some(mut c) = compositor(16, 16) else { return };
        let mut over = solid(2, 16, 16, 0, 0xff, 0);
        // ARGB8888, half transparent.
        over.format = 0;
        for px in over.pixels.chunks_exact_mut(4) {
            px[3] = 0x80;
        }
        c.draw(&[solid(1, 16, 16, 0xff, 0, 0), over]).unwrap();
        let out = c.read_back().unwrap();
        let (r, g, b, _) = at(&out, 16, 8, 8);
        assert!(g > 0x70 && g < 0x90, "half the green is there, got {g:#x}");
        assert!(b > 0x70 && b < 0x90, "half the blue shows through, got {b:#x}");
        assert_eq!(r, 0, "neither had red");
    }

    #[test]
    fn a_stride_wider_than_the_row_is_honoured() {
        let Some(mut c) = compositor(16, 16) else { return };
        // Four pixels a row, but a stride of six pixels: the two extra are
        // padding a client is allowed to leave and must not be shown.
        let (w, h, stride_px) = (4i32, 4i32, 6i32);
        let mut pixels = Vec::new();
        for _ in 0..h {
            for _ in 0..w {
                pixels.extend_from_slice(&[0, 0, 0xff, 0xff]);
            }
            for _ in w..stride_px {
                pixels.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
            }
        }
        let frame = Frame {
            surface: 1,
            width: w,
            height: h,
            stride: stride_px * 4,
            format: 1,
            pixels,
            title: String::new(),
        };
        c.draw(&[frame]).unwrap();
        let out = c.read_back().unwrap();
        // Centred at (6,6) on a 16x16 screen.
        for x in 6..10 {
            assert_eq!(at(&out, 16, x, 7).0, 0xff, "row pixel {x} is red");
        }
        assert_eq!(at(&out, 16, 10, 7), (0, 0, 0, 0xff), "the padding is not drawn");
    }

    #[test]
    fn a_surface_bigger_than_the_screen_is_pinned_to_the_corner() {
        let Some(mut c) = compositor(8, 8) else { return };
        c.draw(&[solid(1, 32, 32, 0, 0, 0xff)]).unwrap();
        let out = c.read_back().unwrap();
        assert_eq!(at(&out, 8, 0, 0).0, 0xff);
        assert_eq!(at(&out, 8, 7, 7).0, 0xff);
    }

    #[test]
    fn redrawing_a_surface_reuses_its_texture() {
        let Some(mut c) = compositor(16, 16) else { return };
        c.draw(&[solid(1, 16, 16, 0, 0, 0xff)]).unwrap();
        c.draw(&[solid(1, 16, 16, 0, 0xff, 0)]).unwrap();
        assert_eq!(c.surfaces.len(), 1, "one surface, one texture");
        let out = c.read_back().unwrap();
        assert_eq!(at(&out, 16, 8, 8).1, 0xff, "the second frame is what shows");
    }
}
