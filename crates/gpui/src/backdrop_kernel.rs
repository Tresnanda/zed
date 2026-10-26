//! Gaussian kernels of the scene backdrop filter, shared by the Metal, wgpu and Direct3D 11 renderers so the three blur
//! the same way. Taps are `[offset, weight, 0, 0]` in texels of the pass's source; the shaders read them as 49 `float4`s.
//! Standalone (no crate imports): the native module's unit tests compile this file on its own with `rustc --test`.

/// One blur pass's taps. `new`: exact reconstruction of the legacy 49 linear samples at pixel-centred output; only
/// sigma <= 8 has a dense support small enough to reduce texture reads. `half`: the half-resolution path (N2).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BackdropKernel {
    /// Taps in use, at most 49.
    pub count: u32,
    /// Keeps `taps` 16-byte aligned for shader constant buffers.
    pub padding: [u32; 3],
    /// `[offset, weight, 0, 0]` per tap.
    pub taps: [[f32; 4]; 49],
}
impl BackdropKernel {
    /// One pass at `sigma` device pixels: the 49 legacy positions, folded into at most 25 linear reads for sigma <= 8.
    pub fn new(sigma: f32, enabled: bool) -> Self {
        let mut result = Self {
            count: 0,
            padding: [0; 3],
            taps: [[0.; 4]; 49],
        };
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
            result.taps[result.count as usize] =
                [pair as f32 - 24.0 + b / weight, weight / total, 0.0, 0.0];
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
    pub fn half(sigma: f32, horizontal: bool) -> Self {
        let mut result = Self {
            count: 0,
            padding: [0; 3],
            taps: [[0.; 4]; 49],
        };
        if !sigma.is_finite() || sigma <= 0. {
            return result;
        }
        let reach = (3. * sigma).ceil() as i32;
        if reach > 60 {
            return Self::new(sigma, true);
        }
        let reach = reach.min(48);
        let weights: Vec<f32> = (-reach..=reach)
            .map(|i| (-0.5 * (i * i) as f32 / (sigma * sigma)).exp())
            .collect();
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
            result.taps[result.count as usize] = [
                (pair as i32 - reach) as f32 + b / (a + b),
                (a + b) / total,
                0.,
                0.,
            ];
            result.count += 1;
        }
        result
    }
}

/// Where and how one backdrop filter samples, for renderers that copy the target region behind it into a scratch first
/// (wgpu, Direct3D 11). Same crop, halo, half-resolution rule and kernels as the Metal renderer's `draw_backdrop`.
#[derive(Clone, Copy, Debug)]
pub struct BackdropPlan {
    /// Device-pixel `[x, y, width, height]` of the target region the passes read: the clipped bounds plus a 3-sigma halo,
    /// inside the target. Copied as is; at half resolution an odd width or height ends in a half block.
    pub crop: [u32; 4],
    /// 2 when the horizontal pass writes a half-size scratch (N2), else 1.
    pub scale: u32,
    /// Device sigma.
    pub sigma: f32,
    /// Horizontal pass taps, in device pixels of the crop (`d * scale` on the half grid).
    pub horizontal: BackdropKernel,
    /// Vertical pass taps, in texels of the horizontal pass's output.
    pub vertical: BackdropKernel,
}

impl BackdropPlan {
    /// `clip` is the filter's bounds intersected with its content mask, device pixels `[x, y, width, height]`; `None` when
    /// it covers nothing of the `target_width` x `target_height` target. `half_resolution` is the
    /// `KYBERN_BACKDROP_HALFRES` switch; it applies to blurs wider than 8 device pixels only.
    pub fn new(
        clip: [f32; 4],
        blur: f32,
        target_width: u32,
        target_height: u32,
        half_resolution: bool,
    ) -> Option<Self> {
        let [x, y, width, height] = clip;
        if !(width > 0. && height > 0.)
            || !blur.is_finite()
            || target_width == 0
            || target_height == 0
        {
            return None;
        }
        let sigma = blur.max(0.01);
        let halo = (sigma * 3.).ceil();
        let left = (x - halo).floor().max(0.);
        let top = (y - halo).floor().max(0.);
        let right = (x + width + halo).ceil().min(target_width as f32);
        let bottom = (y + height + halo).ceil().min(target_height as f32);
        if right <= left || bottom <= top {
            return None;
        }
        // The crop is copied exactly, odd sizes included: rounding it to the half-resolution grid would either reach past
        // the target or leave its last column or row uncopied, where the vertical pass reads the destination. Only the
        // blur scratch rounds up (`scratch_size`); the passes clamp their reads to the copied size.
        let (left, top) = (left as u32, top as u32);
        let (crop_width, crop_height) = (right as u32 - left, bottom as u32 - top);
        // A crop under 2 px has no 2x2 block to average.
        let half = half_resolution && blur > 8. && crop_width >= 2 && crop_height >= 2;
        let scale = if half { 2u32 } else { 1 };
        let (horizontal, vertical) = if half {
            (
                BackdropKernel::half(blur / 2., true),
                BackdropKernel::half(blur / 2., false),
            )
        } else {
            let kernel = BackdropKernel::new(sigma, true);
            (kernel, kernel)
        };
        if horizontal.count == 0 || vertical.count == 0 {
            return None;
        }
        Some(Self {
            crop: [left, top, crop_width, crop_height],
            scale,
            sigma,
            horizontal,
            vertical,
        })
    }
    /// The horizontal pass's output size, `crop / scale` rounded up: an odd last column or row gets its own texel.
    pub fn scratch_size(&self) -> (u32, u32) {
        (
            self.crop[2].div_ceil(self.scale),
            self.crop[3].div_ceil(self.scale),
        )
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
        let values: Vec<_> = (0..97)
            .map(|i| ((i * 13 + i * i * 7) % 97) as f32 / 97.0)
            .collect();
        for sigma in (1..=800)
            .map(|n| n as f32 / 100.)
            .chain([8.01, 16., 40., 128.])
        {
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
                let actual: f32 = kernel.taps[..kernel.count as usize]
                    .iter()
                    .map(|t| sample(&values, centre + t[0]) * t[1])
                    .sum();
                assert!(
                    (actual - expected / total).abs() < 0.00001,
                    "sigma={sigma} centre={centre}"
                );
            }
        }
    }
    #[test]
    fn half_kernels_are_dense_gaussians_within_49_reads() {
        let values: Vec<_> = (0..241)
            .map(|i| ((i * 29 + i * i * 11) % 241) as f32 / 241.0)
            .collect();
        for sigma in [4.01f32, 6., 8., 12., 16., 18., 20.] {
            let reach = ((3. * sigma).ceil() as i32).min(48);
            let total: f32 = (-reach..=reach)
                .map(|i| (-0.5 * (i * i) as f32 / (sigma * sigma)).exp())
                .sum();
            for horizontal in [false, true] {
                let kernel = BackdropKernel::half(sigma, horizontal);
                assert!(kernel.count > 0 && kernel.count <= 49, "sigma={sigma}");
                let sum: f32 = kernel.taps[..kernel.count as usize]
                    .iter()
                    .map(|t| t[1])
                    .sum();
                assert!((sum - 1.).abs() < 1e-5);
                if horizontal && sigma <= 8. {
                    assert!(
                        kernel.taps[..kernel.count as usize]
                            .iter()
                            .all(|t| t[0].fract() == 0.),
                        "sigma={sigma}: block samples must not interpolate"
                    );
                }
                for centre in [60f32, 120., 180.] {
                    let expected: f32 = (-reach..=reach)
                        .map(|i| {
                            values[(centre as i32 + i) as usize]
                                * (-0.5 * (i * i) as f32 / (sigma * sigma)).exp()
                        })
                        .sum::<f32>()
                        / total;
                    let actual: f32 = kernel.taps[..kernel.count as usize]
                        .iter()
                        .map(|t| sample(&values, centre + t[0]) * t[1])
                        .sum();
                    assert!(
                        (actual - expected).abs() < 1e-5,
                        "sigma={sigma} horizontal={horizontal}"
                    );
                }
            }
        }
        assert_eq!(BackdropKernel::half(40., false).count, 49);
        assert_eq!(
            BackdropKernel::half(40., false).taps[1][0],
            BackdropKernel::new(40., true).taps[1][0]
        );
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
    #[test]
    fn plans_crop_the_halo_inside_the_target_on_even_blocks() {
        let plan = BackdropPlan::new([100., 50., 200., 120.], 16., 1000, 800, true).unwrap();
        assert_eq!(plan.scale, 2);
        assert_eq!(plan.crop, [52, 2, 296, 216]);
        assert_eq!(plan.scratch_size(), (148, 108));
        // An odd target narrower than the halo: copied whole, the last column and row in a texel of their own.
        let plan = BackdropPlan::new([0., 0., 33., 33.], 16., 33, 33, true).unwrap();
        assert_eq!((plan.scale, plan.crop), (2, [0, 0, 33, 33]));
        assert_eq!(plan.scratch_size(), (17, 17));
        let plan = BackdropPlan::new([0., 0., 721., 481.], 16., 721, 481, true).unwrap();
        assert_eq!(
            (plan.crop, plan.scratch_size()),
            ([0, 0, 721, 481], (361, 241))
        );
        // An odd crop inside the target is not moved or widened either.
        let plan = BackdropPlan::new([100., 50., 201., 120.], 16., 1000, 800, true).unwrap();
        assert_eq!(
            (plan.crop, plan.scratch_size()),
            ([52, 2, 297, 216], (149, 108))
        );
        // Under 2 px of crop there is no block to average: full resolution, never a 0 px scratch.
        let plan = BackdropPlan::new([0., 0., 50., 1.], 16., 100, 1, true).unwrap();
        assert_eq!(
            (plan.scale, plan.crop, plan.scratch_size()),
            (1, [0, 0, 98, 1], (98, 1))
        );
        let plan = BackdropPlan::new([10.5, 10.5, 20., 20.], 4., 100, 100, true).unwrap();
        assert_eq!((plan.scale, plan.crop), (1, [0, 0, 43, 43]));
        assert!(BackdropPlan::new([0., 0., 0., 10.], 16., 100, 100, true).is_none());
        assert!(BackdropPlan::new([200., 0., 10., 10.], 16., 100, 100, true).is_none());
        assert_eq!(
            BackdropPlan::new([0., 0., 50., 50.], 16., 100, 100, false)
                .unwrap()
                .scale,
            1
        );
        assert!(
            BackdropPlan::new([0., 0., 50., 50.], 0., 100, 100, true)
                .unwrap()
                .horizontal
                .count
                > 0
        );
    }
}
