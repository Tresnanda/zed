// Included in gpui_apple::metal_renderer. Scratch texture is reused across
// surfaces/frames; one ordered command queue prevents concurrent use hazards.
#[repr(C)]
struct BackdropParams { crop:[f32;4], bounds:[f32;4], clip:[f32;4], values:[f32;4], viewport:[f32;4], rounded_clip:u32, scale:f32 }
struct BackdropResources {
    horizontal: metal::RenderPipelineState,
    vertical: metal::RenderPipelineState,
    scratch: Option<metal::Texture>,
    optimized: bool,
    pool: BackdropScratchPool,
}
// One scratch texture per renderer (N1). Sizes round up to 64 px buckets, so a
// menu that animates its bounds does not re-create it each frame. A frame with
// no backdrop marks it purgeable (it leaves the footprint at once but can be
// reused); 30 presented frames without a backdrop release it. It is re-created
// smaller when the largest request over the last 120 frames is under half its
// area. Idea after Zeron's/ZUI's pooled blur target; no code is shared.
const SCRATCH_BUCKET: u64 = 64;
const SCRATCH_RELEASE_FRAMES: u64 = 30;
const SCRATCH_SHRINK_FRAMES: usize = 120;
struct BackdropScratchPool {
    frame: u64,
    last_draw: u64,
    created: u64,
    volatile: bool,
    // Largest (width, height) requested in each of the last 120 frames.
    requests: [(u64, u64); SCRATCH_SHRINK_FRAMES],
}
impl Default for BackdropScratchPool {
    fn default() -> Self { Self { frame: 0, last_draw: 0, created: 0, volatile: false, requests: [(0, 0); SCRATCH_SHRINK_FRAMES] } }
}
fn scratch_bucket(v: u64) -> u64 { v.max(1).div_ceil(SCRATCH_BUCKET) * SCRATCH_BUCKET }
impl BackdropResources {
    fn new(device:&metal::Device, format:metal::MTLPixelFormat, optimized:bool)->Result<Self> {
        let lib=device.new_library_with_source(include_str!("backdrop.metal"),&metal::CompileOptions::new()).map_err(|e|anyhow::anyhow!(e))?;
        // KYBERN_BACKDROP_PROGRAMMABLE=0 forces the fixed-function pass (the control for the alpha check).
        let programmable=device.supports_family(metal::MTLGPUFamily::Apple1)&&!std::env::var("KYBERN_BACKDROP_PROGRAMMABLE").is_ok_and(|v|v=="0");
        let constants=||{let c=metal::FunctionConstantValues::new();c.set_constant_value_at_index(&programmable as *const bool as *const c_void,metal::MTLDataType::Bool,0);c};
        let pipeline=|blend:bool|->Result<metal::RenderPipelineState>{
            let d=metal::RenderPipelineDescriptor::new();
            let vertex=lib.get_function(if optimized {"backdrop_kernel_vertex"} else {"backdrop_vertex"},None).map_err(|e|anyhow::anyhow!(e))?; d.set_vertex_function(Some(&vertex));
            let fragment=lib.get_function(if optimized {"backdrop_kernel_fragment"} else {"backdrop_fragment"},Some(constants())).map_err(|e|anyhow::anyhow!(e))?; d.set_fragment_function(Some(&fragment));
            let a=d.color_attachments().object_at(0).unwrap();a.set_pixel_format(format);
            // Apple GPUs: the vertical pass blends in the shader (`mix(dst, blurred, coverage)` on RGBA, reading the target through
            // `[[color(0)]]`), so the blurred alpha reaches the target; the fixed-function blend (alpha factors Zero / One) kept the
            // destination alpha and left the scene's silhouettes in it. Other GPUs cannot read the target and keep that blend.
            if blend && !programmable {a.set_blending_enabled(true);a.set_source_rgb_blend_factor(metal::MTLBlendFactor::SourceAlpha);a.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);a.set_source_alpha_blend_factor(metal::MTLBlendFactor::Zero);a.set_destination_alpha_blend_factor(metal::MTLBlendFactor::One);}
            device.new_render_pipeline_state(&d).map_err(|e|anyhow::anyhow!(e))
        };
        Ok(Self{horizontal:pipeline(false)?,vertical:pipeline(true)?,scratch:None,optimized,pool:Default::default()})
    }
    /// Once per presented frame, before its batches.
    fn begin_frame(&mut self, has_backdrops: bool) {
        let p = &mut self.pool;
        p.frame += 1;
        p.requests[(p.frame % SCRATCH_SHRINK_FRAMES as u64) as usize] = (0, 0);
        if has_backdrops { return; }
        let Some(scratch) = &self.scratch else { return; };
        if p.frame.saturating_sub(p.last_draw) > SCRATCH_RELEASE_FRAMES {
            self.scratch = None;
            p.volatile = false;
        } else if !p.volatile {
            // In-flight command buffers keep it resident until they complete.
            scratch.set_purgeable_state(metal::MTLPurgeableState::Volatile);
            p.volatile = true;
        }
    }
    /// The scratch for a `w`×`h` request, re-created only to grow, to shrink
    /// (see above), or when the system purged it.
    fn scratch_for(&mut self, device:&metal::Device, format:metal::MTLPixelFormat, w:u64, h:u64) -> &metal::Texture {
        let p = &mut self.pool;
        p.last_draw = p.frame;
        let slot = &mut p.requests[(p.frame % SCRATCH_SHRINK_FRAMES as u64) as usize];
        *slot = (slot.0.max(w), slot.1.max(h));
        if p.volatile {
            p.volatile = false;
            if let Some(scratch) = &self.scratch {
                if scratch.set_purgeable_state(metal::MTLPurgeableState::NonVolatile) == metal::MTLPurgeableState::Empty { self.scratch = None; }
            }
        }
        let (want_w, want_h) = (scratch_bucket(w), scratch_bucket(h));
        let replace = match &self.scratch {
            None => Some((want_w, want_h)),
            Some(t) if t.width() < want_w || t.height() < want_h => Some((want_w.max(t.width()), want_h.max(t.height()))),
            Some(t) if p.frame.saturating_sub(p.created) >= SCRATCH_SHRINK_FRAMES as u64 => {
                let (mw, mh) = p.requests.iter().fold((0, 0), |(a, b), &(x, y)| (a.max(x), b.max(y)));
                let (mw, mh) = (scratch_bucket(mw), scratch_bucket(mh));
                let largest = p.requests.iter().map(|&(x, y)| x * y).max().unwrap_or(0);
                (largest * 2 < t.width() * t.height() && mw * mh < t.width() * t.height()).then_some((mw, mh))
            }
            Some(_) => None,
        };
        if let Some((w, h)) = replace {
            let d=metal::TextureDescriptor::new();d.set_texture_type(metal::MTLTextureType::D2);d.set_pixel_format(format);d.set_width(w);d.set_height(h);d.set_usage(metal::MTLTextureUsage::RenderTarget|metal::MTLTextureUsage::ShaderRead);d.set_storage_mode(metal::MTLStorageMode::Private);
            self.scratch=Some(device.new_texture(&d));
            p.created = p.frame;
        }
        self.scratch.as_ref().unwrap()
    }
}
impl MetalRenderer {
    fn draw_backdrop(&mut self, b:&gpui::Backdrop, target:&metal::TextureRef, viewport:Size<DevicePixels>, command:&metal::CommandBufferRef, rounded_clips:&InstanceBinding)->Result<()> {
        let bounds=b.bounds;let clip=bounds.intersect(&b.content_mask.bounds);
        if clip.is_empty()||b.opacity<=0.0{return Ok(())}
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let enabled = *ENABLED.get_or_init(|| !std::env::var("KYBERN_BACKDROP_KERNEL").is_ok_and(|v| v == "0"));
        // N2: device sigma > 8 blurs a 2×2-prefiltered half-resolution copy at
        // sigma/2 with dense Gaussian taps (BackdropKernel::half), one kernel per
        // pass; KYBERN_BACKDROP_HALFRES=0 keeps full resolution. The legacy control
        // (KERNEL=0) never halves.
        static HALFRES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let half = enabled && *HALFRES.get_or_init(|| !std::env::var("KYBERN_BACKDROP_HALFRES").is_ok_and(|v| v == "0")) && b.blur.as_f32() > 8.;
        let kernel = if enabled {
            let k = if half { BackdropKernel::half(b.blur.as_f32() / 2., true) } else { BackdropKernel::new(b.blur.as_f32().max(0.01), true) };
            (k.count > 0).then_some(k)
        } else { None };
        let vertical_kernel = half.then(|| BackdropKernel::half(b.blur.as_f32() / 2., false));
        let optimized = kernel.is_some();
        static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *TRACE.get_or_init(|| std::env::var("KYBERN_GPU_TRACE").is_ok_and(|v| v == "1")) {
            eprintln!("[backdrop-kernel] sigma={:.4} taps={} enabled={} halfres={} verticalTaps={}", b.blur.as_f32(), kernel.as_ref().map_or(49, |k| k.count), enabled, half, vertical_kernel.as_ref().map_or(0, |k| k.count));
        }
        let sigma=b.blur.as_f32().max(0.01);let halo=(sigma*3.0).ceil();
        // Half-resolution path (N2), set by the kernel hook: 2 for wide blurs.
        let scale=if half {2u64} else {1u64};
        let x=(clip.origin.x.as_f32()-halo).floor().max(0.0);let y=(clip.origin.y.as_f32()-halo).floor().max(0.0);
        let right=(clip.right().as_f32()+halo).ceil().min(target.width() as f32);let bottom=(clip.bottom().as_f32()+halo).ceil().min(target.height() as f32);
        // Even crop at half resolution: every scratch texel covers a full 2×2 block.
        let even=|v:u64|v.div_ceil(scale)*scale;
        let w=even((right-x).max(1.0) as u64);let h=even((bottom-y).max(1.0) as u64);
        if self.backdrop_resources.as_ref().is_none_or(|r| r.optimized != optimized){self.backdrop_resources=Some(BackdropResources::new(&self.device,target.pixel_format(),optimized)?);}
        let r=self.backdrop_resources.as_mut().unwrap();
        let scratch=r.scratch_for(&self.device,target.pixel_format(),w/scale,h/scale).clone();
        let mut p=BackdropParams{crop:[x,y,w as f32,h as f32],bounds:[bounds.origin.x.as_f32(),bounds.origin.y.as_f32(),bounds.size.width.as_f32(),bounds.size.height.as_f32()],clip:[clip.origin.x.as_f32(),clip.origin.y.as_f32(),clip.size.width.as_f32(),clip.size.height.as_f32()],values:[sigma,b.saturation,b.radius.as_f32(),0.0],viewport:[scratch.width() as f32,scratch.height() as f32,b.opacity,b.fade_top],rounded_clip:b.content_mask.rounded_clip,scale:scale as f32};
        let encode=|e:&metal::RenderCommandEncoderRef,p:&BackdropParams,pipeline:&metal::RenderPipelineState,source:&metal::TextureRef|{
            e.set_fragment_buffer(7,Some(&rounded_clips.buffer),rounded_clips.offset as u64);
            if let Some(kernel) = vertical_kernel.as_ref().filter(|_| p.values[3] != 0.0).or(kernel.as_ref()) {
                e.set_fragment_bytes(1,mem::size_of::<BackdropKernel>() as u64,kernel as *const _ as *const c_void);
            }
            e.set_render_pipeline_state(pipeline);e.set_vertex_bytes(0,mem::size_of::<BackdropParams>() as u64,p as *const _ as *const c_void);e.set_fragment_bytes(0,mem::size_of::<BackdropParams>() as u64,p as *const _ as *const c_void);e.set_fragment_texture(0,Some(source));e.draw_primitives(metal::MTLPrimitiveType::TriangleStrip,0,4);e.end_encoding();
        };
        let e=new_command_encoder_for_texture(command,&scratch,size(gpui::DevicePixels(scratch.width() as i32),gpui::DevicePixels(scratch.height() as i32)),Some(metal::MTLClearColor::new(0.,0.,0.,0.)));
        encode(e,&p,&r.horizontal,target);
        p.values[3]=1.0;p.viewport[0]=target.width() as f32;p.viewport[1]=target.height() as f32;
        let e=new_command_encoder_for_texture(command,target,viewport,None);encode(e,&p,&r.vertical,&scratch);
        Ok(())
    }
}


// Exact reconstruction of the legacy 49 linear samples at pixel-centred output.
// Only sigma <= 8 has a dense support small enough to reduce texture reads.
#[repr(C)]
#[derive(Clone, Copy)]
struct BackdropKernel {
    count: u32,
    padding: [u32; 3],
    taps: [[f32; 4]; 49],
}
impl BackdropKernel {
    fn new(sigma: f32, enabled: bool) -> Self {
        let mut result = Self { count: 0, padding: [0; 3], taps: [[0.; 4]; 49] };
        if !enabled || !sigma.is_finite() || sigma <= 0. {
            return result;
        }
        if sigma > 8. {
            let mut total = 0.0;
            for (index, i) in (-24..=24).enumerate() {
                let d = i as f32 * (sigma / 8.0).max(0.125);
                let w = (-0.5 * d * d / (sigma * sigma).max(0.001)).exp();
                result.taps[index] = [d, w, 0.0, 0.0];
                total += w;
            }
            for tap in &mut result.taps {
                tap[1] /= total;
            }
            result.count = 49;
            return result;
        }
        let mut weights = [0.0f32; 49];
        let mut total = 0.0;
        for i in -24..=24 {
            let d = i as f32 * (sigma / 8.0).max(0.125);
            let w = (-0.5 * d * d / (sigma * sigma).max(0.001)).exp();
            let lo = d.floor();
            let fraction = d - lo;
            let index = (lo as i32 + 24) as usize;
            weights[index] += w * (1.0 - fraction);
            if fraction > 0.0 {
                weights[index + 1] += w * fraction;
            }
            total += w;
        }
        // Pair adjacent texels using the texture unit's linear interpolation.
        // Clamp-to-edge commutes with this weighted reconstruction at crop edges.
        for pair in (0..49).step_by(2) {
            let a = weights[pair];
            let b = weights.get(pair + 1).copied().unwrap_or(0.0);
            let weight = a + b;
            if weight == 0.0 {
                continue;
            }
            result.taps[result.count as usize] = [pair as f32 - 24.0 + b / weight, weight / total, 0.0, 0.0];
            result.count += 1;
        }
        result
    }
    /// One pass of the half-resolution path (N2) at `sigma` in half-size texels.
    /// Dense Gaussian taps out to 3 sigma, cut at 48 texels (2.4 sigma at most,
    /// up to sigma 20) so they fold into at most 49 linear reads; wider blurs keep
    /// the sparse 49 positions of `new`. The horizontal pass reads the full-size
    /// target, where a linear read between two half texels is not their mix, so
    /// it folds only when the unfolded taps would not fit (sigma > 8).
    fn half(sigma: f32, horizontal: bool) -> Self {
        let mut result = Self { count: 0, padding: [0; 3], taps: [[0.; 4]; 49] };
        if !sigma.is_finite() || sigma <= 0. {
            return result;
        }
        let reach = (3. * sigma).ceil() as i32;
        if reach > 60 {
            return Self::new(sigma, true);
        }
        let reach = reach.min(48);
        let weights: Vec<f32> = (-reach..=reach).map(|i| (-0.5 * (i * i) as f32 / (sigma * sigma)).exp()).collect();
        let total: f32 = weights.iter().sum();
        if horizontal && weights.len() <= 49 {
            for (index, w) in weights.iter().enumerate() {
                result.taps[index] = [(index as i32 - reach) as f32, w / total, 0., 0.];
            }
            result.count = weights.len() as u32;
            return result;
        }
        for pair in (0..weights.len()).step_by(2) {
            let a = weights[pair];
            let b = weights.get(pair + 1).copied().unwrap_or(0.);
            result.taps[result.count as usize] = [(pair as i32 - reach) as f32 + b / (a + b), (a + b) / total, 0., 0.];
            result.count += 1;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(values: &[f32], x: f32) -> f32 {
        let lo = x.floor();
        let f = x - lo;
        let at = |i: f32| values[(i as i32).clamp(0, values.len() as i32 - 1) as usize];
        at(lo) * (1.0 - f) + at(lo + 1.0) * f
    }
    #[test]
    fn reconstructed_kernel_preserves_linear_samples_including_edges() {
        let values: Vec<_> = (0..97).map(|i| ((i * 13 + i * i * 7) % 97) as f32 / 97.0).collect();
        for sigma in (1..=800).map(|n| n as f32 / 100.).chain([8.01, 16., 40., 128.]) {
            let kernel = BackdropKernel::new(sigma, true);
            assert!(kernel.count > 0 && kernel.count <= (if sigma <= 8. { 25 } else { 49 }));
            for centre in [0., 1., 4., 48., 94., 96.] {
                let mut expected = 0.;
                let mut total = 0.;
                for i in -24..=24 {
                    let d = i as f32 * (sigma / 8.).max(0.125);
                    let w = (-0.5 * d * d / (sigma * sigma).max(0.001)).exp();
                    expected += sample(&values, centre + d) * w;
                    total += w;
                }
                let actual: f32 = kernel.taps[..kernel.count as usize].iter().map(|t| sample(&values, centre + t[0]) * t[1]).sum();
                assert!((actual - expected / total).abs() < 0.00001, "sigma={sigma} centre={centre}");
            }
        }
    }
    #[test]
    fn half_kernels_are_dense_gaussians_within_49_reads() {
        let values: Vec<_> = (0..241).map(|i| ((i * 29 + i * i * 11) % 241) as f32 / 241.0).collect();
        for sigma in [4.01f32, 6., 8., 12., 16., 18., 20.] {
            let reach = ((3. * sigma).ceil() as i32).min(48);
            let total: f32 = (-reach..=reach).map(|i| (-0.5 * (i * i) as f32 / (sigma * sigma)).exp()).sum();
            for horizontal in [false, true] {
                let kernel = BackdropKernel::half(sigma, horizontal);
                assert!(kernel.count > 0 && kernel.count <= 49, "sigma={sigma}");
                let sum: f32 = kernel.taps[..kernel.count as usize].iter().map(|t| t[1]).sum();
                assert!((sum - 1.).abs() < 1e-5);
                if horizontal && sigma <= 8. {
                    assert!(kernel.taps[..kernel.count as usize].iter().all(|t| t[0].fract() == 0.), "sigma={sigma}: block samples must not interpolate");
                }
                for centre in [60f32, 120., 180.] {
                    let expected: f32 = (-reach..=reach).map(|i| values[(centre as i32 + i) as usize] * (-0.5 * (i * i) as f32 / (sigma * sigma)).exp()).sum::<f32>() / total;
                    let actual: f32 = kernel.taps[..kernel.count as usize].iter().map(|t| sample(&values, centre + t[0]) * t[1]).sum();
                    assert!((actual - expected).abs() < 1e-5, "sigma={sigma} horizontal={horizontal}");
                }
            }
        }
        assert_eq!(BackdropKernel::half(40., false).count, 49);
        assert_eq!(BackdropKernel::half(40., false).taps[1][0], BackdropKernel::new(40., true).taps[1][0]);
    }
    #[test]
    fn dense_kernel_is_bounded_and_wide_blur_retains_original_positions() {
        assert_eq!(std::mem::size_of::<BackdropKernel>(), 800);
        assert!(BackdropKernel::new(2., true).count <= 7);
        for sigma in [0., -1., f32::NAN, f32::INFINITY] {
            assert_eq!(BackdropKernel::new(sigma, true).count, 0);
        }
        assert_eq!(BackdropKernel::new(1., false).count, 0);
        for sigma in [8.01, 16., 40., 128.] {
            assert_eq!(BackdropKernel::new(sigma, true).count, 49);
        }
    }
}
