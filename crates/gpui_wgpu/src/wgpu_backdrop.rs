//! Kybern: the scene backdrop filter on wgpu (`backdrop.wgsl`), the counterpart of gpui_apple's `backdrop_renderer.rs`.
//!
//! Per filter: copy the target region behind it (the clip plus a 3-sigma halo) into a scratch, blur it horizontally into a
//! second scratch (half size for wide blurs, `KYBERN_BACKDROP_HALFRES=0` keeps full size), then draw the vertical pass
//! into the target. Metal samples the target directly; a swap chain image cannot be bound as a texture here, so the copy
//! is the price, and it is cropped to the filter, never the whole frame. Every filter of a frame shares one uniform
//! buffer (dynamic offsets) and the two scratch textures, each pooled like Metal's (N1).

use gpui::backdrop_kernel::{BackdropKernel, BackdropPlan};
use std::num::NonZeroU64;

const SCRATCH_BUCKET: u32 = 64;
const SCRATCH_RELEASE_FRAMES: u64 = 30;
const SCRATCH_SHRINK_FRAMES: usize = 120;
/// `backdrop.wgsl` `Params`: six vec4s and 49 taps.
const PARAMS_SIZE: u64 = 6 * 16 + 49 * 16;

/// `KYBERN_BACKDROP_FILTER=0` skips the filter (the translucent fill above it still paints): the before/after control.
pub(crate) fn backdrop_filter_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED
        .get_or_init(|| !std::env::var("KYBERN_BACKDROP_FILTER").is_ok_and(|value| value == "0"))
}

fn half_resolution() -> bool {
    static HALF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HALF.get_or_init(|| !std::env::var("KYBERN_BACKDROP_HALFRES").is_ok_and(|value| value == "0"))
}

struct Pipelines {
    horizontal: wgpu::RenderPipeline,
    vertical: wgpu::RenderPipeline,
    params_layout: wgpu::BindGroupLayout,
    textures_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

struct Scratch {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

fn bucket(value: u32) -> u32 {
    value.max(1).div_ceil(SCRATCH_BUCKET) * SCRATCH_BUCKET
}

/// One pooled scratch texture: sizes round up to 64 px buckets, it is re-created to grow, or smaller when the largest
/// request of the last 120 frames is under half its area. The copy and the blur scratch are pooled apart, since at half
/// resolution the blur scratch needs a quarter of the copy's texels.
struct Pooled {
    scratch: Option<Scratch>,
    size: (u32, u32),
    created: u64,
    requests: [(u32, u32); SCRATCH_SHRINK_FRAMES],
}

impl Default for Pooled {
    fn default() -> Self {
        Self {
            scratch: None,
            size: (0, 0),
            created: 0,
            requests: [(0, 0); SCRATCH_SHRINK_FRAMES],
        }
    }
}

impl Pooled {
    /// Record this frame's largest request and return the size to re-create the texture at, if it must change.
    fn resize(
        &mut self,
        frame: u64,
        slot: usize,
        (want_width, want_height): (u32, u32),
    ) -> Option<(u32, u32)> {
        let request = &mut self.requests[slot];
        *request = (request.0.max(want_width), request.1.max(want_height));
        let (width, height) = self.size;
        if self.scratch.is_none() {
            Some((bucket(want_width), bucket(want_height)))
        } else if width < want_width || height < want_height {
            Some((
                bucket(want_width).max(width),
                bucket(want_height).max(height),
            ))
        } else if frame.saturating_sub(self.created) >= SCRATCH_SHRINK_FRAMES as u64 {
            let (most_width, most_height) = self
                .requests
                .iter()
                .fold((0, 0), |(a, b), &(x, y)| (a.max(x), b.max(y)));
            let largest = self
                .requests
                .iter()
                .map(|&(x, y)| u64::from(x) * u64::from(y))
                .max()
                .unwrap_or(0);
            let area = u64::from(width) * u64::from(height);
            let (most_width, most_height) = (bucket(most_width), bucket(most_height));
            (largest * 2 < area && u64::from(most_width) * u64::from(most_height) < area)
                .then_some((most_width, most_height))
        } else {
            None
        }
    }

    fn bytes(&self) -> u64 {
        if self.scratch.is_none() {
            return 0;
        }
        4 * u64::from(self.size.0) * u64::from(self.size.1)
    }
}

#[derive(Default)]
pub(crate) struct WgpuBackdrop {
    pipelines: Option<Pipelines>,
    /// The copied target region (full resolution, the largest crop).
    copy: Pooled,
    /// The horizontal pass's output (the largest `BackdropPlan::scratch_size`).
    blur: Pooled,
    frame: u64,
    last_draw: u64,
    uniforms: Option<wgpu::Buffer>,
    uniform_stride: u64,
    /// This frame's parameters, kept to reuse the allocation.
    uniform_data: Vec<u8>,
    params_bind_group: Option<wgpu::BindGroup>,
    /// What `params_bind_group` binds: the uniform buffer, the chain buffer and its size in use.
    params_bound: Option<(wgpu::Buffer, wgpu::Buffer, u64)>,
    horizontal_textures: Option<wgpu::BindGroup>,
    vertical_textures: Option<wgpu::BindGroup>,
    /// This frame's plan per `Scene::backdrops` index; `None` draws nothing for that filter.
    plans: Vec<Option<BackdropPlan>>,
}

impl WgpuBackdrop {
    fn pipelines(device: &wgpu::Device, format: wgpu::TextureFormat) -> Pipelines {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("backdrop_shaders"),
            source: wgpu::ShaderSource::Wgsl(include_str!("backdrop.wgsl").into()),
        });
        let params_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("backdrop_params_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: NonZeroU64::new(PARAMS_SIZE),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let textures_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("backdrop_textures_layout"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("backdrop_layout"),
            bind_group_layouts: &[Some(&params_layout), Some(&textures_layout)],
            immediate_size: 0,
        });
        // No blending: the vertical pass writes `mix(destination, blurred, coverage)` itself.
        let pipeline = |label: &str, vertex: &str, fragment: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(vertex),
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fragment),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let horizontal = pipeline(
            "backdrop_horizontal",
            "vs_backdrop_horizontal",
            "fs_backdrop_horizontal",
        );
        let vertical = pipeline(
            "backdrop_vertical",
            "vs_backdrop_vertical",
            "fs_backdrop_vertical",
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("backdrop_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Pipelines {
            horizontal,
            vertical,
            params_layout,
            textures_layout,
            sampler,
        }
    }

    /// Plan this frame's filters, upload their parameters and size the scratches. Call once per frame before encoding;
    /// `rounded_clips` is the frame's chain buffer (`rounded_clips_size` bytes in use).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        backdrops: &[gpui::Backdrop],
        target_width: u32,
        target_height: u32,
        rounded_clips: &wgpu::Buffer,
        rounded_clips_size: u64,
    ) {
        self.frame += 1;
        let slot = (self.frame % SCRATCH_SHRINK_FRAMES as u64) as usize;
        self.copy.requests[slot] = (0, 0);
        self.blur.requests[slot] = (0, 0);
        self.plans.clear();
        let half = half_resolution();
        for backdrop in backdrops {
            let clip = backdrop.bounds.intersect(&backdrop.content_mask.bounds);
            let plan = (backdrop.opacity > 0.0 && !clip.is_empty())
                .then(|| {
                    BackdropPlan::new(
                        [
                            clip.origin.x.0,
                            clip.origin.y.0,
                            clip.size.width.0,
                            clip.size.height.0,
                        ],
                        backdrop.blur.0,
                        target_width,
                        target_height,
                        half,
                    )
                })
                .flatten();
            self.plans.push(plan);
        }
        if self.plans.iter().all(Option::is_none) {
            if self.frame.saturating_sub(self.last_draw) > SCRATCH_RELEASE_FRAMES {
                self.copy.scratch = None;
                self.blur.scratch = None;
                self.horizontal_textures = None;
                self.vertical_textures = None;
                self.params_bind_group = None;
                self.params_bound = None;
            }
            return;
        }
        self.last_draw = self.frame;
        let (copy_want, blur_want) = self.plans.iter().flatten().fold(
            ((0, 0), (0, 0)),
            |((copy_width, copy_height), (blur_width, blur_height)), plan| {
                let (width, height) = plan.scratch_size();
                (
                    (copy_width.max(plan.crop[2]), copy_height.max(plan.crop[3])),
                    (blur_width.max(width), blur_height.max(height)),
                )
            },
        );

        let pipelines = self
            .pipelines
            .get_or_insert_with(|| Self::pipelines(device, format));
        let scratch = |label, (width, height), usage| {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | usage,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            Scratch { texture, view }
        };
        let mut replaced = false;
        if let Some(size) = self.copy.resize(self.frame, slot, copy_want) {
            self.copy.scratch = Some(scratch(
                "backdrop_copy",
                size,
                wgpu::TextureUsages::COPY_DST,
            ));
            self.copy.size = size;
            self.copy.created = self.frame;
            replaced = true;
        }
        if let Some(size) = self.blur.resize(self.frame, slot, blur_want) {
            self.blur.scratch = Some(scratch(
                "backdrop_blur",
                size,
                wgpu::TextureUsages::RENDER_ATTACHMENT,
            ));
            self.blur.size = size;
            self.blur.created = self.frame;
            replaced = true;
        }
        if let (true, Some(copy), Some(blur)) = (replaced, &self.copy.scratch, &self.blur.scratch) {
            let textures = |label, source: &Scratch, destination: &Scratch| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(label),
                    layout: &pipelines.textures_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&source.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&destination.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&pipelines.sampler),
                        },
                    ],
                })
            };
            self.horizontal_textures = Some(textures("backdrop_horizontal_textures", copy, copy));
            self.vertical_textures = Some(textures("backdrop_vertical_textures", blur, copy));
        }

        // Two entries per filter: horizontal, then vertical.
        let alignment = u64::from(device.limits().min_uniform_buffer_offset_alignment);
        self.uniform_stride = PARAMS_SIZE.next_multiple_of(alignment);
        let needed = self.uniform_stride * 2 * backdrops.len() as u64;
        let data = &mut self.uniform_data;
        data.clear();
        data.resize(needed as usize, 0);
        for (index, (backdrop, plan)) in backdrops.iter().zip(&self.plans).enumerate() {
            let Some(plan) = plan else { continue };
            for (pass, kernel) in [&plan.horizontal, &plan.vertical].into_iter().enumerate() {
                let offset = (self.uniform_stride * (2 * index + pass) as u64) as usize;
                write_params(
                    &mut data[offset..offset + PARAMS_SIZE as usize],
                    backdrop,
                    plan,
                    kernel,
                    target_width,
                    target_height,
                );
            }
        }
        let fits = self
            .uniforms
            .as_ref()
            .is_some_and(|buffer| buffer.size() >= needed);
        if !fits {
            self.uniforms = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("backdrop_uniforms"),
                size: needed.next_power_of_two().max(self.uniform_stride * 8),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        let Some(uniforms) = self.uniforms.as_ref() else {
            return;
        };
        queue.write_buffer(uniforms, 0, data);
        // The chain buffer can be re-created (and grown) between frames: rebind only when it or the uniforms changed.
        let bound = (uniforms.clone(), rounded_clips.clone(), rounded_clips_size);
        if self.params_bind_group.is_some() && self.params_bound.as_ref() == Some(&bound) {
            return;
        }
        self.params_bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("backdrop_params"),
            layout: &pipelines.params_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: uniforms,
                        offset: 0,
                        size: NonZeroU64::new(PARAMS_SIZE),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: rounded_clips,
                        offset: 0,
                        size: NonZeroU64::new(rounded_clips_size),
                    }),
                },
            ],
        }));
        self.params_bound = Some(bound);
    }

    /// Encode filter `index` of this frame's `Scene::backdrops` (between render passes): copy, horizontal, vertical.
    pub(crate) fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        index: usize,
        target: &wgpu::Texture,
        target_view: &wgpu::TextureView,
    ) {
        let Some(Some(plan)) = self.plans.get(index) else {
            return;
        };
        let (
            Some(pipelines),
            Some(copy),
            Some(blur),
            Some(params),
            Some(horizontal_textures),
            Some(vertical_textures),
        ) = (
            &self.pipelines,
            &self.copy.scratch,
            &self.blur.scratch,
            &self.params_bind_group,
            &self.horizontal_textures,
            &self.vertical_textures,
        )
        else {
            return;
        };
        let [x, y, width, height] = plan.crop;
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            copy.texture.as_image_copy(),
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let (blur_width, blur_height) = plan.scratch_size();
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("backdrop_horizontal"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &blur.view,
                    resolve_target: None,
                    // The pass rewrites every texel of its viewport, and nothing outside it is read.
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            pass.set_viewport(0.0, 0.0, blur_width as f32, blur_height as f32, 0.0, 1.0);
            pass.set_scissor_rect(0, 0, blur_width, blur_height);
            pass.set_pipeline(&pipelines.horizontal);
            pass.set_bind_group(
                0,
                params,
                &[(self.uniform_stride * 2 * index as u64) as u32],
            );
            pass.set_bind_group(1, horizontal_textures, &[]);
            pass.draw(0..4, 0..1);
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("backdrop_vertical"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });
        pass.set_pipeline(&pipelines.vertical);
        pass.set_bind_group(
            0,
            params,
            &[(self.uniform_stride * (2 * index + 1) as u64) as u32],
        );
        pass.set_bind_group(1, vertical_textures, &[]);
        pass.draw(0..4, 0..1);
    }

    /// Bytes held by the scratch textures (the memory counters).
    #[allow(dead_code)]
    pub(crate) fn scratch_bytes(&self) -> u64 {
        self.copy.bytes() + self.blur.bytes()
    }
}

fn write_params(
    out: &mut [u8],
    backdrop: &gpui::Backdrop,
    plan: &BackdropPlan,
    kernel: &BackdropKernel,
    target_width: u32,
    target_height: u32,
) {
    let clip = backdrop.bounds.intersect(&backdrop.content_mask.bounds);
    let bounds = backdrop.bounds;
    let floats: [[f32; 4]; 5] = [
        plan.crop.map(|value| value as f32),
        [
            bounds.origin.x.0,
            bounds.origin.y.0,
            bounds.size.width.0,
            bounds.size.height.0,
        ],
        [
            clip.origin.x.0,
            clip.origin.y.0,
            clip.size.width.0,
            clip.size.height.0,
        ],
        [
            plan.sigma,
            backdrop.saturation,
            backdrop.radius.0,
            plan.scale as f32,
        ],
        [
            target_width as f32,
            target_height as f32,
            backdrop.opacity,
            backdrop.fade_top,
        ],
    ];
    let mut cursor = 0;
    let mut put = |bytes: [u8; 4]| {
        out[cursor..cursor + 4].copy_from_slice(&bytes);
        cursor += 4;
    };
    for value in floats.iter().flatten() {
        put(value.to_ne_bytes());
    }
    for value in [backdrop.content_mask.rounded_clip, kernel.count, 0, 0] {
        put(value.to_ne_bytes());
    }
    for value in kernel.taps.iter().flatten() {
        put(value.to_ne_bytes());
    }
}
