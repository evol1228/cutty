//! # cutty-gpu
//!
//! The GPU compositor: all pixel work outside of decode/encode runs here,
//! via wgpu (Vulkan-first on Linux). One compositor serves both render
//! frontends — realtime preview and offline export — which is what makes
//! *preview == export* hold by construction.
//!
//! The unit of work is [`Compositor::composite`]: an ordered list of
//! [`Layer`]s (bottom → top), each an RGBA source texture with a
//! placement (center/size/rotation in output pixels), an opacity, and a
//! [`BlendMode`], rendered over black into an offscreen target at a given
//! resolution, then copied to a mappable staging buffer for readback.
//!
//! Targets carry **two staging slots** so a caller can pipeline: submit
//! frame N+1 while frame N's readback is still in flight (the export
//! frontend does this; preview uses a single slot synchronously).
//!
//! Compositing happens in sRGB-encoded RGBA8 (`Rgba8Unorm`, no implicit
//! linearization) — see `composite.wgsl` for the linear-light upgrade
//! note.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Errors from GPU initialization and compositing.
#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    #[error("no usable GPU adapter found (tried Vulkan first, then all backends): {0}")]
    AdapterNotFound(String),
    #[error("GPU device request failed: {0}")]
    RequestDevice(String),
    #[error("GPU poll failed: {0}")]
    Poll(String),
    #[error("GPU readback failed: {0}")]
    Readback(String),
}

/// How a layer combines with the accumulated layers below it. Mirrors the
/// engine's clip-level enum; kept separate so this crate stays free of
/// editor model types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendMode {
    #[default]
    Normal,
    Multiply,
    Screen,
    Overlay,
    Add,
}

impl BlendMode {
    fn shader_id(self) -> u32 {
        match self {
            BlendMode::Normal => 0,
            BlendMode::Multiply => 1,
            BlendMode::Screen => 2,
            BlendMode::Overlay => 3,
            BlendMode::Add => 4,
        }
    }
}

/// An RGBA8 source texture (one per decoded source), updated in place as
/// new frames are decoded.
pub struct SourceTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

impl SourceTexture {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

/// One layer of a composite: a source texture placed on the output.
///
/// Placement is in **output pixels**: the source rectangle is scaled to
/// `size`, rotated by `rotation_rad` (clockwise, y-down) about its
/// center, and its center put at `center`. The caller does all editor
/// semantics (project-space fitting etc.) — this crate is pure geometry.
pub struct Layer<'a> {
    pub source: &'a SourceTexture,
    /// Center of the placed rectangle, output pixels.
    pub center: (f32, f32),
    /// Size of the placed rectangle, output pixels.
    pub size: (f32, f32),
    /// Clockwise rotation about the center, radians.
    pub rotation_rad: f32,
    /// 0.0..=1.0.
    pub opacity: f32,
    pub blend: BlendMode,
}

/// Per-layer uniform data, `repr(C)` to match `LayerUniform` in WGSL.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct LayerUniform {
    inv_row0: [f32; 4],
    inv_row1: [f32; 4],
    opacity: f32,
    blend: u32,
    _pad: [f32; 2],
}

impl LayerUniform {
    /// Build the inverse placement (output pixel center → source UV) from
    /// a layer's forward placement.
    fn from_layer(layer: &Layer) -> Option<Self> {
        let (cx, cy) = layer.center;
        let (w, h) = layer.size;
        if !(w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite()) {
            return None; // degenerate placement: contributes nothing
        }
        let (sin, cos) = layer.rotation_rad.sin_cos();
        Some(Self {
            inv_row0: [cos / w, sin / w, (-cos * cx - sin * cy) / w + 0.5, 0.0],
            inv_row1: [-sin / h, cos / h, (sin * cx - cos * cy) / h + 0.5, 0.0],
            opacity: layer.opacity.clamp(0.0, 1.0),
            blend: layer.blend.shader_id(),
            _pad: [0.0; 2],
        })
    }
}

/// State of one readback slot.
struct Slot {
    staging: wgpu::Buffer,
    mapped: Arc<AtomicBool>,
    submission: Option<wgpu::SubmissionIndex>,
}

/// An offscreen composite target: ping-pong color textures plus two
/// readback slots. Create one per output resolution and reuse it across
/// frames.
pub struct Target {
    width: u32,
    height: u32,
    /// Bytes per row in the staging buffers (`width * 4` aligned to 256).
    padded_bytes_per_row: u32,
    color: [wgpu::Texture; 2],
    views: [wgpu::TextureView; 2],
    slots: [Slot; 2],
    uniforms: wgpu::Buffer,
    uniform_capacity: u32,
}

impl Target {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bytes per row of the frames handed out by [`Compositor::read_slot`].
    pub fn stride(&self) -> usize {
        self.padded_bytes_per_row as usize
    }
}

/// Number of readback slots per target (double buffering).
pub const SLOTS: usize = 2;

const UNIFORM_SIZE: u64 = std::mem::size_of::<LayerUniform>() as u64;

/// The wgpu device plus the one compositing pipeline. Each render
/// frontend owns its own `Compositor` (they live on different threads);
/// instances are independent.
pub struct Compositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_stride: u32,
    adapter_info: wgpu::AdapterInfo,
}

impl Compositor {
    /// Bring up the GPU: Vulkan first (the primary Linux path), falling
    /// back to any available backend (e.g. GL) so odd setups still
    /// composite. Headless — no surface, offscreen targets only.
    pub fn new() -> Result<Self, GpuError> {
        match Self::with_backends(wgpu::Backends::VULKAN) {
            Ok(c) => Ok(c),
            Err(vulkan_err) => Self::with_backends(wgpu::Backends::all()).map_err(|e| {
                GpuError::AdapterNotFound(format!("vulkan: {vulkan_err}; all backends: {e}"))
            }),
        }
    }

    fn with_backends(backends: wgpu::Backends) -> Result<Self, GpuError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: None, // headless: offscreen targets only
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        }))
        .map_err(|e| GpuError::AdapterNotFound(e.to_string()))?;

        // Broadly-compatible limits, but with the adapter's real texture
        // resolution caps — downlevel defaults top out at 2048² which
        // can't even hold a 4K export target.
        let limits = wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits());
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("cutty-compositor"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            experimental_features: Default::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::default(),
        }))
        .map_err(|e| GpuError::RequestDevice(e.to_string()))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cutty-composite"),
            source: wgpu::ShaderSource::Wgsl(include_str!("composite.wgsl").into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("cutty-composite-bind"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(UNIFORM_SIZE),
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
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("cutty-composite-pl"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("cutty-composite-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("cutty-composite-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let align = device.limits().min_uniform_buffer_offset_alignment;
        let uniform_stride = (UNIFORM_SIZE as u32).div_ceil(align) * align;

        Ok(Self {
            adapter_info: adapter.get_info(),
            device,
            queue,
            pipeline,
            bind_layout,
            sampler,
            uniform_stride,
        })
    }

    /// Human-readable adapter description (for logs).
    pub fn adapter_label(&self) -> String {
        format!(
            "{} ({:?})",
            self.adapter_info.name, self.adapter_info.backend
        )
    }

    /// Create a source texture for decoded RGBA frames of the given size.
    pub fn create_source(&self, width: u32, height: u32) -> SourceTexture {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cutty-source"),
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
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        SourceTexture {
            texture,
            view,
            width,
            height,
        }
    }

    /// Upload an RGBA frame (rows `stride` bytes apart) into a source
    /// texture. `data` must cover `height` rows of at least `width * 4`
    /// bytes each.
    pub fn upload_rgba(&self, target: &SourceTexture, data: &[u8], stride: usize) {
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &target.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride as u32),
                rows_per_image: Some(target.height),
            },
            wgpu::Extent3d {
                width: target.width,
                height: target.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Create a composite target of the given size (with both readback
    /// slots).
    pub fn create_target(&self, width: u32, height: u32) -> Target {
        let make_color = || {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("cutty-target"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let color = [make_color(), make_color()];
        let views = [
            color[0].create_view(&wgpu::TextureViewDescriptor::default()),
            color[1].create_view(&wgpu::TextureViewDescriptor::default()),
        ];
        let padded_bytes_per_row = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let make_slot = || Slot {
            staging: self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("cutty-readback"),
                size: u64::from(padded_bytes_per_row) * u64::from(height),
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            mapped: Arc::new(AtomicBool::new(false)),
            submission: None,
        };
        let uniform_capacity = 8u32;
        let uniforms = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cutty-layer-uniforms"),
            size: u64::from(uniform_capacity * self.uniform_stride),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Target {
            width,
            height,
            padded_bytes_per_row,
            color,
            views,
            slots: [make_slot(), make_slot()],
            uniforms,
            uniform_capacity,
        }
    }

    /// Composite `layers` (bottom → top) over black into `target` and
    /// start an asynchronous readback into staging slot `slot`. Retrieve
    /// the pixels with [`Compositor::read_slot`].
    ///
    /// Submissions are pipelined: a second `composite` on the *other*
    /// slot may be issued before the first slot is read.
    pub fn composite(&self, target: &mut Target, layers: &[Layer], slot: usize) {
        assert!(slot < SLOTS, "slot out of range");

        // Degenerate placements contribute nothing; drop them up front so
        // pass count == uniform count.
        let uniforms: Vec<LayerUniform> =
            layers.iter().filter_map(LayerUniform::from_layer).collect();
        let kept: Vec<&Layer> = layers
            .iter()
            .filter(|l| LayerUniform::from_layer(l).is_some())
            .collect();

        if uniforms.len() as u32 > target.uniform_capacity {
            let capacity = (uniforms.len() as u32).next_power_of_two();
            target.uniforms = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("cutty-layer-uniforms"),
                size: u64::from(capacity * self.uniform_stride),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            target.uniform_capacity = capacity;
        }
        for (i, u) in uniforms.iter().enumerate() {
            self.queue.write_buffer(
                &target.uniforms,
                u64::from(i as u32 * self.uniform_stride),
                bytemuck::bytes_of(u),
            );
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("cutty-composite"),
            });

        // Clear the first accumulator to opaque black (the canvas).
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("cutty-clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target.views[0],
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
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
            multiview_mask: None,
        });

        // One full-target pass per layer, ping-ponging accumulators.
        for (i, layer) in kept.iter().enumerate() {
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("cutty-composite-bind"),
                layout: &self.bind_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &target.uniforms,
                            offset: 0,
                            size: wgpu::BufferSize::new(UNIFORM_SIZE),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&target.views[i % 2]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&layer.source.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("cutty-layer"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.views[(i + 1) % 2],
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[i as u32 * self.uniform_stride]);
            pass.draw(0..3, 0..1);
        }

        // Copy the final accumulator into the slot's staging buffer.
        let final_view = kept.len() % 2;
        let slot_state = &mut target.slots[slot];
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target.color[final_view],
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &slot_state.staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(target.padded_bytes_per_row),
                    rows_per_image: Some(target.height),
                },
            },
            wgpu::Extent3d {
                width: target.width,
                height: target.height,
                depth_or_array_layers: 1,
            },
        );

        let submission = self.queue.submit([encoder.finish()]);

        slot_state.mapped.store(false, Ordering::SeqCst);
        let flag = slot_state.mapped.clone();
        slot_state
            .staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                if result.is_ok() {
                    flag.store(true, Ordering::SeqCst);
                }
            });
        slot_state.submission = Some(submission);
    }

    /// Block until slot `slot`'s readback is complete and hand the padded
    /// RGBA rows to `f` (row pitch = [`Target::stride`]). Waits only for
    /// the submission that filled this slot, so work queued after it keeps
    /// running (that is the double-buffering win).
    pub fn read_slot<R>(
        &self,
        target: &mut Target,
        slot: usize,
        f: impl FnOnce(&[u8], usize) -> R,
    ) -> Result<R, GpuError> {
        assert!(slot < SLOTS, "slot out of range");
        let stride = target.padded_bytes_per_row as usize;
        let slot_state = &mut target.slots[slot];
        let submission = slot_state
            .submission
            .take()
            .ok_or_else(|| GpuError::Readback("slot has no pending composite".into()))?;

        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            })
            .map_err(|e| GpuError::Poll(e.to_string()))?;
        // The map callback fires during poll; the wait above covers the
        // copy, so one extra full wait is only ever needed if a driver
        // signals the fence before running callbacks.
        if !slot_state.mapped.load(Ordering::SeqCst) {
            self.device
                .poll(wgpu::PollType::wait_indefinitely())
                .map_err(|e| GpuError::Poll(e.to_string()))?;
        }
        if !slot_state.mapped.load(Ordering::SeqCst) {
            return Err(GpuError::Readback("buffer mapping never completed".into()));
        }

        let result = {
            let range = slot_state
                .staging
                .slice(..)
                .get_mapped_range()
                .map_err(|e| GpuError::Readback(e.to_string()))?;
            f(&range, stride)
        };
        slot_state.staging.unmap();
        Ok(result)
    }

    /// Convenience for callers without pipelining needs: composite and
    /// read back synchronously via slot 0.
    pub fn composite_and_read<R>(
        &self,
        target: &mut Target,
        layers: &[Layer],
        f: impl FnOnce(&[u8], usize) -> R,
    ) -> Result<R, GpuError> {
        self.composite(target, layers, 0);
        self.read_slot(target, 0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GPU tests need an adapter; skip (visibly) where none exists so the
    /// suite stays green on headless CI boxes.
    fn compositor() -> Option<Compositor> {
        match Compositor::new() {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("cutty-gpu tests: skipping, no adapter ({e})");
                None
            }
        }
    }

    /// A `w`×`h` RGBA texture filled with one color.
    fn solid(comp: &Compositor, w: u32, h: u32, rgba: [u8; 4]) -> SourceTexture {
        let tex = comp.create_source(w, h);
        let data: Vec<u8> = rgba
            .iter()
            .copied()
            .cycle()
            .take((w * h * 4) as usize)
            .collect();
        comp.upload_rgba(&tex, &data, (w * 4) as usize);
        tex
    }

    fn full_frame_layer<'a>(tex: &'a SourceTexture, w: u32, h: u32) -> Layer<'a> {
        Layer {
            source: tex,
            center: (w as f32 / 2.0, h as f32 / 2.0),
            size: (w as f32, h as f32),
            rotation_rad: 0.0,
            opacity: 1.0,
            blend: BlendMode::Normal,
        }
    }

    /// Read pixel (x, y) from a padded readback.
    fn px(data: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
        let i = y * stride + x * 4;
        [data[i], data[i + 1], data[i + 2], data[i + 3]]
    }

    #[test]
    fn empty_composite_is_opaque_black() {
        let Some(comp) = compositor() else { return };
        let mut target = comp.create_target(8, 8);
        comp.composite_and_read(&mut target, &[], |data, stride| {
            for y in 0..8 {
                for x in 0..8 {
                    assert_eq!(px(data, stride, x, y), [0, 0, 0, 255]);
                }
            }
        })
        .unwrap();
    }

    /// A layer covering the target 1:1 must read back bit-identically —
    /// pixel centers map to texel centers, so bilinear filtering is exact.
    #[test]
    fn identity_placement_is_bit_exact() {
        let Some(comp) = compositor() else { return };
        let (w, h) = (16u32, 8u32);
        let tex = comp.create_source(w, h);
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                data[i] = (x * 16) as u8;
                data[i + 1] = (y * 32) as u8;
                data[i + 2] = 200;
                data[i + 3] = 255;
            }
        }
        comp.upload_rgba(&tex, &data, (w * 4) as usize);

        let mut target = comp.create_target(w, h);
        comp.composite_and_read(
            &mut target,
            &[full_frame_layer(&tex, w, h)],
            |out, stride| {
                for y in 0..h as usize {
                    for x in 0..w as usize {
                        let want = {
                            let i = (y * w as usize + x) * 4;
                            [data[i], data[i + 1], data[i + 2], 255]
                        };
                        assert_eq!(px(out, stride, x, y), want, "pixel ({x}, {y})");
                    }
                }
            },
        )
        .unwrap();
    }

    /// Every blend mode against a CPU reference on solid colors (interior
    /// pixels only — edges carry the one-texel AA ramp).
    #[test]
    fn blend_modes_match_cpu_reference() {
        let Some(comp) = compositor() else { return };
        let (w, h) = (8u32, 8u32);
        let bottom_rgb = [200u8, 60, 120];
        let top_rgb = [90u8, 180, 40];
        let opacity = 0.6f64;

        let bottom = solid(
            &comp,
            w,
            h,
            [bottom_rgb[0], bottom_rgb[1], bottom_rgb[2], 255],
        );
        let top = solid(&comp, w, h, [top_rgb[0], top_rgb[1], top_rgb[2], 255]);

        let reference = |mode: BlendMode, b: f64, s: f64| -> f64 {
            let blended = match mode {
                BlendMode::Normal => s,
                BlendMode::Multiply => s * b,
                BlendMode::Screen => s + b - s * b,
                BlendMode::Overlay => {
                    if b <= 0.5 {
                        2.0 * s * b
                    } else {
                        1.0 - 2.0 * (1.0 - s) * (1.0 - b)
                    }
                }
                BlendMode::Add => (s + b).min(1.0),
            };
            b + (blended - b) * opacity
        };

        for mode in [
            BlendMode::Normal,
            BlendMode::Multiply,
            BlendMode::Screen,
            BlendMode::Overlay,
            BlendMode::Add,
        ] {
            let mut target = comp.create_target(w, h);
            let layers = [
                full_frame_layer(&bottom, w, h),
                Layer {
                    opacity: opacity as f32,
                    blend: mode,
                    ..full_frame_layer(&top, w, h)
                },
            ];
            comp.composite_and_read(&mut target, &layers, |out, stride| {
                let got = px(out, stride, 4, 4);
                for c in 0..3 {
                    let b = f64::from(bottom_rgb[c]) / 255.0;
                    let s = f64::from(top_rgb[c]) / 255.0;
                    let want = (reference(mode, b, s) * 255.0).round();
                    let diff = (f64::from(got[c]) - want).abs();
                    assert!(
                        diff <= 1.0,
                        "{mode:?} channel {c}: got {} want {want}",
                        got[c]
                    );
                }
            })
            .unwrap();
        }
    }

    /// Placement: a layer covering the right half paints only there;
    /// opacity mixes toward the backdrop.
    #[test]
    fn placement_and_opacity() {
        let Some(comp) = compositor() else { return };
        let (w, h) = (16u32, 8u32);
        let tex = solid(&comp, 8, 8, [255, 255, 255, 255]);
        let mut target = comp.create_target(w, h);
        let layers = [Layer {
            source: &tex,
            center: (12.0, 4.0), // right half: x in [8, 16)
            size: (8.0, 8.0),
            rotation_rad: 0.0,
            opacity: 0.5,
            blend: BlendMode::Normal,
        }];
        comp.composite_and_read(&mut target, &layers, |out, stride| {
            assert_eq!(px(out, stride, 3, 4), [0, 0, 0, 255], "left: untouched");
            let got = px(out, stride, 12, 4);
            for c in 0..3 {
                assert!(
                    (i16::from(got[c]) - 128).abs() <= 1,
                    "right: 50% white, got {got:?}"
                );
            }
        })
        .unwrap();
    }

    /// 90° clockwise rotation moves a marker from the layer's +x edge to
    /// the output's +y edge.
    #[test]
    fn rotation_is_clockwise_y_down() {
        let Some(comp) = compositor() else { return };
        let size = 9u32; // odd: an exact center pixel
                         // Texture: black with a white column at the right edge.
        let tex = comp.create_source(size, size);
        let mut data = vec![0u8; (size * size * 4) as usize];
        for y in 0..size {
            for x in 0..size {
                let i = ((y * size + x) * 4) as usize;
                data[i + 3] = 255;
                if x == size - 1 {
                    data[i] = 255;
                    data[i + 1] = 255;
                    data[i + 2] = 255;
                }
            }
        }
        comp.upload_rgba(&tex, &data, (size * 4) as usize);

        let mut target = comp.create_target(size, size);
        let layers = [Layer {
            source: &tex,
            center: (4.5, 4.5),
            size: (size as f32, size as f32),
            rotation_rad: std::f32::consts::FRAC_PI_2,
            opacity: 1.0,
            blend: BlendMode::Normal,
        }];
        comp.composite_and_read(&mut target, &layers, |out, stride| {
            // The white +x edge must now sit at the *bottom* (+y edge).
            let bottom = px(out, stride, 4, 8);
            let right = px(out, stride, 8, 4);
            assert!(bottom[0] > 200, "white edge rotated to bottom: {bottom:?}");
            assert!(right[0] < 50, "right edge is now dark: {right:?}");
        })
        .unwrap();
    }

    /// Double-buffered readback: two composites in flight on different
    /// slots read back their own (different) contents.
    #[test]
    fn double_buffered_slots_are_independent() {
        let Some(comp) = compositor() else { return };
        let (w, h) = (8u32, 8u32);
        let red = solid(&comp, w, h, [255, 0, 0, 255]);
        let blue = solid(&comp, w, h, [0, 0, 255, 255]);
        let mut target = comp.create_target(w, h);

        comp.composite(&mut target, &[full_frame_layer(&red, w, h)], 0);
        comp.composite(&mut target, &[full_frame_layer(&blue, w, h)], 1);

        comp.read_slot(&mut target, 0, |out, stride| {
            assert_eq!(px(out, stride, 4, 4), [255, 0, 0, 255]);
        })
        .unwrap();
        comp.read_slot(&mut target, 1, |out, stride| {
            assert_eq!(px(out, stride, 4, 4), [0, 0, 255, 255]);
        })
        .unwrap();
    }

    /// Layer stacking order: later layers paint over earlier ones.
    #[test]
    fn later_layers_paint_on_top() {
        let Some(comp) = compositor() else { return };
        let (w, h) = (8u32, 8u32);
        let red = solid(&comp, w, h, [255, 0, 0, 255]);
        let green = solid(&comp, w, h, [0, 255, 0, 255]);
        let mut target = comp.create_target(w, h);
        let layers = [full_frame_layer(&red, w, h), full_frame_layer(&green, w, h)];
        comp.composite_and_read(&mut target, &layers, |out, stride| {
            assert_eq!(px(out, stride, 4, 4), [0, 255, 0, 255]);
        })
        .unwrap();
    }
}
