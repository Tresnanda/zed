// Kybern: the scene backdrop filter on Direct3D 11 (`backdrop.hlsl`), the counterpart of gpui_apple's
// `backdrop_renderer.rs` and gpui_wgpu's `wgpu_backdrop.rs`. Included in `directx_renderer.rs`.
//
// Per filter: copy the back buffer region behind it (the clip plus a 3-sigma halo) into a scratch, blur it horizontally
// into a second scratch (half size for wide blurs, `KYBERN_BACKDROP_HALFRES=0` keeps full size), then draw the vertical
// pass into the back buffer. The scratches are pooled like Metal's (N1): 64 px buckets, released after 30 frames without
// a filter, re-created smaller when the last 120 frames needed under half their area.

use gpui::backdrop_kernel::{BackdropKernel, BackdropPlan};

const BACKDROP_SCRATCH_BUCKET: u32 = 64;
const BACKDROP_RELEASE_FRAMES: u64 = 30;
const BACKDROP_SHRINK_FRAMES: usize = 120;

/// `backdrop.hlsl` `BackdropParams`.
#[repr(C)]
#[derive(Clone, Copy)]
struct BackdropParams {
    crop: [f32; 4],
    bounds: [f32; 4],
    clip: [f32; 4],
    values: [f32; 4],
    target_info: [f32; 4],
    info: [u32; 4],
    taps: [[f32; 4]; 49],
}

struct BackdropScratch {
    copy: ID3D11Texture2D,
    copy_view: Option<ID3D11ShaderResourceView>,
    blur_target: Option<ID3D11RenderTargetView>,
    blur_view: Option<ID3D11ShaderResourceView>,
    width: u32,
    height: u32,
}

struct DirectXBackdrop {
    horizontal_vertex: ID3D11VertexShader,
    horizontal_fragment: ID3D11PixelShader,
    vertical_vertex: ID3D11VertexShader,
    vertical_fragment: ID3D11PixelShader,
    params: Option<ID3D11Buffer>,
    sampler: Option<ID3D11SamplerState>,
    no_blend: ID3D11BlendState,
    scratch: Option<BackdropScratch>,
    frame: u64,
    last_draw: u64,
    created: u64,
    requests: [(u32, u32); BACKDROP_SHRINK_FRAMES],
}

/// `KYBERN_BACKDROP_FILTER=0` skips the filter (the translucent fill above it still paints): the before/after control.
fn backdrop_filter_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| !std::env::var("KYBERN_BACKDROP_FILTER").is_ok_and(|value| value == "0"))
}

fn backdrop_half_resolution() -> bool {
    static HALF: OnceLock<bool> = OnceLock::new();
    *HALF.get_or_init(|| !std::env::var("KYBERN_BACKDROP_HALFRES").is_ok_and(|value| value == "0"))
}

fn backdrop_bucket(value: u32) -> u32 {
    value.max(1).div_ceil(BACKDROP_SCRATCH_BUCKET) * BACKDROP_SCRATCH_BUCKET
}

impl DirectXBackdrop {
    fn new(device: &ID3D11Device) -> Result<Self> {
        let vertex = |module| -> Result<ID3D11VertexShader> {
            let raw = RawShaderBytes::new(module, ShaderTarget::Vertex)?;
            create_vertex_shader(device, raw.as_bytes())
        };
        let fragment = |module| -> Result<ID3D11PixelShader> {
            let raw = RawShaderBytes::new(module, ShaderTarget::Fragment)?;
            create_fragment_shader(device, raw.as_bytes())
        };
        let sampler = unsafe {
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MipLODBias: 0.0,
                MaxAnisotropy: 1,
                ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                BorderColor: [0.0; 4],
                MinLOD: 0.0,
                MaxLOD: D3D11_FLOAT32_MAX,
            };
            let mut output = None;
            device.CreateSamplerState(&desc, Some(&mut output))?;
            output
        };
        // The vertical pass writes `lerp(destination, blurred, coverage)` itself.
        let no_blend = unsafe {
            let mut desc = D3D11_BLEND_DESC::default();
            desc.RenderTarget[0].BlendEnable = false.into();
            desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
            let mut state = None;
            device.CreateBlendState(&desc, Some(&mut state))?;
            state.context("creating the backdrop blend state")?
        };
        Ok(Self {
            horizontal_vertex: vertex(ShaderModule::BackdropHorizontal)?,
            horizontal_fragment: fragment(ShaderModule::BackdropHorizontal)?,
            vertical_vertex: vertex(ShaderModule::BackdropVertical)?,
            vertical_fragment: fragment(ShaderModule::BackdropVertical)?,
            params: create_constant_buffer::<BackdropParams>(device)?,
            sampler,
            no_blend,
            scratch: None,
            frame: 0,
            last_draw: 0,
            created: 0,
            requests: [(0, 0); BACKDROP_SHRINK_FRAMES],
        })
    }

    /// Once per frame, before its batches.
    fn begin_frame(&mut self, has_backdrops: bool) {
        self.frame += 1;
        self.requests[(self.frame % BACKDROP_SHRINK_FRAMES as u64) as usize] = (0, 0);
        if !has_backdrops && self.frame.saturating_sub(self.last_draw) > BACKDROP_RELEASE_FRAMES {
            self.scratch = None;
        }
    }

    /// Size the scratches for a `width` x `height` crop: re-created only to grow or to shrink (see above).
    fn reserve_scratch(&mut self, device: &ID3D11Device, width: u32, height: u32) -> Result<()> {
        self.last_draw = self.frame;
        let slot = &mut self.requests[(self.frame % BACKDROP_SHRINK_FRAMES as u64) as usize];
        *slot = (slot.0.max(width), slot.1.max(height));
        let replace = match &self.scratch {
            None => Some((backdrop_bucket(width), backdrop_bucket(height))),
            Some(s) if s.width < width || s.height < height => Some((
                backdrop_bucket(width).max(s.width),
                backdrop_bucket(height).max(s.height),
            )),
            Some(s) if self.frame.saturating_sub(self.created) >= BACKDROP_SHRINK_FRAMES as u64 => {
                let (most_width, most_height) = self
                    .requests
                    .iter()
                    .fold((0, 0), |(a, b), &(x, y)| (a.max(x), b.max(y)));
                let (most_width, most_height) =
                    (backdrop_bucket(most_width), backdrop_bucket(most_height));
                let largest = self
                    .requests
                    .iter()
                    .map(|&(x, y)| u64::from(x) * u64::from(y))
                    .max()
                    .unwrap_or(0);
                let area = u64::from(s.width) * u64::from(s.height);
                (largest * 2 < area && u64::from(most_width) * u64::from(most_height) < area)
                    .then_some((most_width, most_height))
            }
            Some(_) => None,
        };
        if let Some((width, height)) = replace {
            self.scratch = None;
            let texture = |bind: u32| -> Result<ID3D11Texture2D> {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: RENDER_TARGET_FORMAT,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: bind,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut output = None;
                unsafe { device.CreateTexture2D(&desc, None, Some(&mut output))? };
                output.context("creating a backdrop scratch")
            };
            let copy = texture(D3D11_BIND_SHADER_RESOURCE.0 as u32)?;
            let blur = texture((D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32)?;
            let (mut copy_view, mut blur_view, mut blur_target) = (None, None, None);
            unsafe {
                device.CreateShaderResourceView(&copy, None, Some(&mut copy_view))?;
                device.CreateShaderResourceView(&blur, None, Some(&mut blur_view))?;
                device.CreateRenderTargetView(&blur, None, Some(&mut blur_target))?;
            }
            self.scratch = Some(BackdropScratch {
                copy,
                copy_view,
                blur_target,
                blur_view,
                width,
                height,
            });
            self.created = self.frame;
        }
        Ok(())
    }
}

fn backdrop_params(
    backdrop: &gpui::Backdrop,
    plan: &BackdropPlan,
    kernel: &BackdropKernel,
    viewport: &D3D11_VIEWPORT,
) -> BackdropParams {
    let clip = backdrop.bounds.intersect(&backdrop.content_mask.bounds);
    let bounds = backdrop.bounds;
    BackdropParams {
        crop: plan.crop.map(|value| value as f32),
        bounds: [
            bounds.origin.x.0,
            bounds.origin.y.0,
            bounds.size.width.0,
            bounds.size.height.0,
        ],
        clip: [
            clip.origin.x.0,
            clip.origin.y.0,
            clip.size.width.0,
            clip.size.height.0,
        ],
        values: [
            plan.sigma,
            backdrop.saturation,
            backdrop.radius.0,
            plan.scale as f32,
        ],
        target_info: [
            viewport.Width,
            viewport.Height,
            backdrop.opacity,
            backdrop.fade_top,
        ],
        info: [backdrop.content_mask.rounded_clip, kernel.count, 0, 0],
        taps: kernel.taps,
    }
}

impl DirectXRenderer {
    /// Copy, horizontal pass, vertical pass; leaves the back buffer, its viewport and the sprite slots as the batches
    /// expect them.
    fn draw_backdrop(&mut self, backdrop: &gpui::Backdrop) -> Result<()> {
        let clip = backdrop.bounds.intersect(&backdrop.content_mask.bounds);
        if !backdrop_filter_enabled() || clip.is_empty() || backdrop.opacity <= 0.0 {
            return Ok(());
        }
        if self.backdrop.is_none() {
            let device = self
                .devices
                .as_ref()
                .context("devices missing")?
                .device
                .clone();
            self.backdrop =
                Some(DirectXBackdrop::new(&device).context("creating the backdrop filter")?);
        }
        let devices = self.devices.as_ref().context("devices missing")?;
        let resources = self.resources.as_ref().context("resources missing")?;
        let viewport = resources.viewport;
        let Some(plan) = BackdropPlan::new(
            [
                clip.origin.x.0,
                clip.origin.y.0,
                clip.size.width.0,
                clip.size.height.0,
            ],
            backdrop.blur.0,
            viewport.Width as u32,
            viewport.Height as u32,
            backdrop_half_resolution(),
        ) else {
            return Ok(());
        };
        let render_target = resources
            .render_target
            .as_ref()
            .context("render target missing")?;
        let render_target_view = resources.render_target_view.clone();
        let backdrop_resources = self.backdrop.as_mut().context("backdrop filter missing")?;
        let [x, y, width, height] = plan.crop;
        backdrop_resources.reserve_scratch(&devices.device, width, height)?;
        let backdrop_resources = &*backdrop_resources;
        let scratch = backdrop_resources
            .scratch
            .as_ref()
            .context("backdrop scratch missing")?;
        let (blur_width, blur_height) = plan.scratch_size();
        let context = &devices.device_context;
        let params = backdrop_resources
            .params
            .as_ref()
            .context("backdrop parameters missing")?;
        unsafe {
            context.CopySubresourceRegion(
                &scratch.copy,
                0,
                0,
                0,
                0,
                render_target,
                0,
                Some(&D3D11_BOX {
                    left: x,
                    top: y,
                    front: 0,
                    right: x + width,
                    bottom: y + height,
                    back: 1,
                }),
            );
            update_buffer(
                context,
                params,
                &[backdrop_params(
                    backdrop,
                    &plan,
                    &plan.horizontal,
                    &viewport,
                )],
            )?;
            context.OMSetRenderTargets(Some(slice::from_ref(&scratch.blur_target)), None);
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: blur_width as f32,
                Height: blur_height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            context.OMSetBlendState(&backdrop_resources.no_blend, None, 0xFFFFFFFF);
            context.VSSetShader(&backdrop_resources.horizontal_vertex, None);
            context.PSSetShader(&backdrop_resources.horizontal_fragment, None);
            context.VSSetConstantBuffers(2, Some(slice::from_ref(&backdrop_resources.params)));
            context.PSSetConstantBuffers(2, Some(slice::from_ref(&backdrop_resources.params)));
            context.PSSetSamplers(1, Some(slice::from_ref(&backdrop_resources.sampler)));
            context.PSSetShaderResources(
                3,
                Some(&[scratch.copy_view.clone(), scratch.copy_view.clone()]),
            );
            context.Draw(4, 0);

            update_buffer(
                context,
                params,
                &[backdrop_params(backdrop, &plan, &plan.vertical, &viewport)],
            )?;
            context.OMSetRenderTargets(Some(slice::from_ref(&render_target_view)), None);
            context.RSSetViewports(Some(slice::from_ref(&viewport)));
            context.VSSetShader(&backdrop_resources.vertical_vertex, None);
            context.PSSetShader(&backdrop_resources.vertical_fragment, None);
            context.PSSetShaderResources(
                3,
                Some(&[scratch.blur_view.clone(), scratch.copy_view.clone()]),
            );
            context.Draw(4, 0);
            // The blur scratch is a render target again next time, and the copy a copy destination.
            context.PSSetShaderResources(3, Some(&[None, None]));
        }
        Ok(())
    }
}
