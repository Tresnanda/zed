// Included in gpui_apple::metal_renderer (compositor_loops_gpui.py). One plain CALayer per promoted loop part,
// a sublayer of the Metal layer, filled with the part's colour: a fill keeps its corner radius; a sprite is masked
// by its own coverage tile, copied once from the shared-storage glyph atlas (the shader's `color.a *= coverage`).
// A repeating CA animation reproduces the native curve (`frame_gpui.py`, `paint_text_gpui.py`, `rotate_gpui.py`):
// matrix rest 0.22 -> 1 at 15% -> 0.22 at 45% and breathe 1 -> 0.45 -> 1 (both ease-in-out) on `opacity`, the
// shimmer's 1 -> 1.6 -> 1 alpha gain sampled into keyframes, rotate one clockwise turn per cycle. WindowServer runs it.
#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {
    fn CACurrentMediaTime() -> f64;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGColorCreateSRGB(red: f64, green: f64, blue: f64, alpha: f64) -> *mut c_void;
    fn CGColorRelease(color: *mut c_void);
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGColorSpaceRelease(space: *mut c_void);
    fn CGDataProviderCreateWithCFData(data: *const c_void) -> *mut c_void;
    fn CGDataProviderRelease(provider: *mut c_void);
    fn CGImageCreate(width: usize, height: usize, bits_per_component: usize, bits_per_pixel: usize, bytes_per_row: usize, space: *mut c_void,
        bitmap_info: u32, provider: *mut c_void, decode: *const f64, should_interpolate: bool, intent: i32) -> *mut c_void;
    fn CGImageRelease(image: *mut c_void);
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: isize) -> *const c_void;
    fn CFRelease(value: *const c_void);
}
type CompositorKey = (u64, u32);
impl Drop for MetalRenderer {
    fn drop(&mut self) {
        for (_, (_, layer)) in self.compositor_layers.drain() {
            unsafe { let _: () = msg_send![layer, removeFromSuperlayer]; let _: () = msg_send![layer, release]; }
        }
    }
}
/// The shimmer's alpha gain (`1 + 0.6 * ease(triangle)`) as a fraction of its peak, after the scene's clamp to 1.
fn compositor_shimmer(alpha: f32, phase: f64) -> f64 {
    let bez = |t: f64, a: f64, b: f64| 3.0 * a * t * (1.0 - t).powi(2) + 3.0 * b * t * t * (1.0 - t) + t * t * t;
    let ease = |x: f64| { let (mut lo, mut hi) = (0.0, 1.0); for _ in 0..24 { let mid = (lo + hi) * 0.5; if bez(mid, 0.42, 0.58) < x { lo = mid; } else { hi = mid; } } bez((lo + hi) * 0.5, 0.0, 1.0) };
    let gain = 1.0 + 0.6 * ease(if phase < 0.5 { phase * 2.0 } else { 2.0 - phase * 2.0 });
    let peak = (alpha as f64 * 1.6).min(1.0);
    if peak <= 0.0 { 1.0 } else { (alpha as f64 * gain).min(1.0) / peak }
}
impl MetalRenderer {
    fn compositor_in_sync(&self, scene: &Scene) -> bool {
        scene.compositor_loops.len() == self.compositor_layers.len()
            && scene.compositor_loops.iter().all(|l| self.compositor_layers.get(&(l.id, l.part)).is_some_and(|(shown, _)| shown == l))
    }
    fn compositor_frame(&self, bounds: Bounds<ScaledPixels>) -> Option<(cocoa::foundation::NSRect, f64)> {
        let layer = self.layer.as_ref()?;
        let drawable = layer.drawable_size();
        let frame: cocoa::foundation::NSRect = unsafe { msg_send![&**layer, bounds] };
        if frame.size.width <= 0. || frame.size.height <= 0. || drawable.width <= 0. { return None; }
        let sx = drawable.width / frame.size.width;
        let sy = drawable.height / frame.size.height;
        let flipped: bool = unsafe { msg_send![&**layer, isGeometryFlipped] };
        let (x, y, w, h) = (bounds.origin.x.0 as f64, bounds.origin.y.0 as f64, bounds.size.width.0 as f64, bounds.size.height.0 as f64);
        let top = if flipped { y / sy } else { frame.size.height - (y + h) / sy };
        Some((cocoa::foundation::NSRect::new(cocoa::foundation::NSPoint::new(frame.origin.x + x / sx, frame.origin.y + top), NSSize::new(w / sx, h / sy)), sx))
    }
    /// White premultiplied coverage of a monochrome atlas tile, as a CGImage (caller releases).
    fn compositor_mask_image(&self, tile: &gpui::AtlasTile) -> Option<*mut c_void> {
        let texture = self.sprite_atlas.metal_texture(tile.texture_id);
        if texture.pixel_format() != MTLPixelFormat::A8Unorm && texture.pixel_format() != MTLPixelFormat::R8Unorm { return None; }
        let (x, y) = (tile.bounds.origin.x.0.max(0) as u64, tile.bounds.origin.y.0.max(0) as u64);
        let (w, h) = (tile.bounds.size.width.0.max(0) as u64, tile.bounds.size.height.0.max(0) as u64);
        if w == 0 || h == 0 || x + w > texture.width() || y + h > texture.height() { return None; }
        let mut coverage = vec![0u8; (w * h) as usize];
        let region = metal::MTLRegion { origin: metal::MTLOrigin { x, y, z: 0 }, size: metal::MTLSize { width: w, height: h, depth: 1 } };
        texture.get_bytes(coverage.as_mut_ptr() as *mut c_void, w, region, 0);
        let rgba: Vec<u8> = coverage.iter().flat_map(|&a| [a, a, a, a]).collect();
        unsafe {
            let data = CFDataCreate(ptr::null(), rgba.as_ptr(), rgba.len() as isize);
            let provider = CGDataProviderCreateWithCFData(data);
            let space = CGColorSpaceCreateDeviceRGB();
            // kCGImageAlphaPremultipliedLast | kCGBitmapByteOrderDefault
            let image = CGImageCreate(w as usize, h as usize, 8, 32, (w * 4) as usize, space, 1, provider, ptr::null(), false, 0);
            CGColorSpaceRelease(space);
            CGDataProviderRelease(provider);
            CFRelease(data);
            (!image.is_null()).then_some(image)
        }
    }
    unsafe fn compositor_animation(l: &gpui::CompositorLoop, layer: *mut objc::runtime::Object) -> *mut objc::runtime::Object {
        let number = |v: f64| -> *mut objc::runtime::Object { msg_send![objc::class!(NSNumber), numberWithDouble: v] };
        let array = |items: &[*mut objc::runtime::Object]| -> *mut objc::runtime::Object {
            msg_send![objc::class!(NSArray), arrayWithObjects: items.as_ptr() count: items.len() as u64]
        };
        let string = |s: &std::ffi::CStr| -> *mut objc::runtime::Object { msg_send![objc::class!(NSString), stringWithUTF8String: s.as_ptr()] };
        // kCAMediaTimingFunctionEaseInEaseOut is (0.42, 0, 0.58, 1), CSS `ease-in-out`.
        let named = |name: &std::ffi::CStr| -> *mut objc::runtime::Object { msg_send![objc::class!(CAMediaTimingFunction), functionWithName: string(name)] };
        let animation: *mut objc::runtime::Object;
        let mut delay = l.delay_ms / 1000.;
        if let Some(highlight) = l.band {
            // A banded shimmer glyph: its colour, sampled at its own position across the label (`shimmer_band_weight`), linear.
            animation = msg_send![objc::class!(CAKeyframeAnimation), animationWithKeyPath: string(c"backgroundColor")];
            let n = 48;
            let colors: Vec<*mut objc::runtime::Object> = (0..=n).map(|k| {
                let c: gpui::Rgba = gpui::shimmer_band_mix(l.color, highlight, gpui::shimmer_band_weight(l.band_u, k as f64 / n as f64)).into();
                CGColorCreateSRGB(c.r as f64, c.g as f64, c.b as f64, c.a as f64) as *mut objc::runtime::Object
            }).collect();
            let _: () = msg_send![animation, setValues: array(&colors)];
            for color in colors { CGColorRelease(color as *mut c_void); }
            let times: Vec<_> = (0..=n).map(|k| number(k as f64 / n as f64)).collect();
            let _: () = msg_send![animation, setKeyTimes: array(&times)];
        } else if l.rotate {
            // GPUI turns clockwise on screen; an unflipped CA layer turns clockwise for negative angles.
            animation = msg_send![objc::class!(CABasicAnimation), animationWithKeyPath: string(c"transform.rotation.z")];
            let _: () = msg_send![animation, setFromValue: number(0.)];
            let _: () = msg_send![animation, setToValue: number(-std::f64::consts::TAU)];
            let _: () = msg_send![animation, setTimingFunction: named(c"linear")];
            delay = 0.;
        } else {
            animation = msg_send![objc::class!(CAKeyframeAnimation), animationWithKeyPath: string(c"opacity")];
            let ease = named(c"easeInEaseOut");
            let (values, times, curves): (Vec<f64>, Vec<f64>, Vec<*mut objc::runtime::Object>) = if l.shimmer {
                let n = 32;
                ((0..=n).map(|k| compositor_shimmer(l.color.a, k as f64 / n as f64)).collect(), (0..=n).map(|k| k as f64 / n as f64).collect(), Vec::new())
            } else if l.breathe {
                (vec![1., 0.45, 1.], vec![0., 0.5, 1.], vec![ease, ease])
            } else {
                (vec![0.22, 1., 0.22, 0.22], vec![0., 0.15, 0.45, 1.], vec![ease, ease, named(c"linear")])
            };
            let values: Vec<_> = values.into_iter().map(number).collect();
            let times: Vec<_> = times.into_iter().map(number).collect();
            let _: () = msg_send![animation, setValues: array(&values)];
            let _: () = msg_send![animation, setKeyTimes: array(&times)];
            if !curves.is_empty() { let _: () = msg_send![animation, setTimingFunctions: array(&curves)]; }
        }
        let _: () = msg_send![animation, setDuration: l.cycle_ms / 1000.];
        let _: () = msg_send![animation, setRepeatCount: f32::INFINITY];
        let _: () = msg_send![animation, setRemovedOnCompletion: NO];
        // Phase-align with the scene's clock: local time zero is the leaf's start plus its delay.
        let elapsed = l.started.elapsed().as_secs_f64() - delay;
        let now: f64 = msg_send![layer, convertTime: CACurrentMediaTime() fromLayer: ptr::null_mut::<objc::runtime::Object>()];
        let _: () = msg_send![animation, setBeginTime: now - elapsed];
        animation
    }
    fn sync_compositor_layers(&mut self, scene: &Scene) {
        let Some(parent) = self.layer.clone() else { return };
        let wanted: std::collections::HashSet<CompositorKey> = scene.compositor_loops.iter().map(|l| (l.id, l.part)).collect();
        let gone: Vec<CompositorKey> = self.compositor_layers.keys().filter(|key| !wanted.contains(key)).copied().collect();
        for key in gone {
            if let Some((_, layer)) = self.compositor_layers.remove(&key) {
                unsafe { let _: () = msg_send![layer, removeFromSuperlayer]; let _: () = msg_send![layer, release]; }
            }
        }
        for l in scene.compositor_loops.iter() {
            let Some((frame, scale)) = self.compositor_frame(l.bounds) else { continue };
            let existing = self.compositor_layers.get(&(l.id, l.part)).map(|(shown, layer)| (*shown, *layer));
            let layer = existing.map_or_else(|| unsafe {
                let layer: *mut objc::runtime::Object = msg_send![objc::class!(CALayer), new];
                let _: () = msg_send![&*parent, addSublayer: layer];
                layer
            }, |(_, layer)| layer);
            // The shimmer's layer carries its clamped peak alpha; its opacity animates down from there.
            let mut rgba: gpui::Rgba = l.color.into();
            if l.shimmer && l.band.is_none() { rgba.a = (l.color.a * 1.6).min(1.0); }
            unsafe {
                let _: () = msg_send![layer, setFrame: frame];
                let _: () = msg_send![layer, setCornerRadius: l.radius as f64 / scale];
                let color = CGColorCreateSRGB(rgba.r as f64, rgba.g as f64, rgba.b as f64, rgba.a as f64);
                let _: () = msg_send![layer, setBackgroundColor: color];
                CGColorRelease(color);
                if existing.is_none_or(|(shown, _)| shown.tile != l.tile || shown.bounds.size != l.bounds.size) {
                    let mask: *mut objc::runtime::Object = match l.tile.and_then(|tile| self.compositor_mask_image(&tile)) {
                        Some(image) => {
                            let mask: *mut objc::runtime::Object = msg_send![objc::class!(CALayer), layer];
                            let bounds = cocoa::foundation::NSRect::new(cocoa::foundation::NSPoint::new(0., 0.), frame.size);
                            let _: () = msg_send![mask, setFrame: bounds];
                            let _: () = msg_send![mask, setContentsScale: scale];
                            let _: () = msg_send![mask, setContents: image as *mut objc::runtime::Object];
                            CGImageRelease(image);
                            mask
                        }
                        None => ptr::null_mut(),
                    };
                    let _: () = msg_send![layer, setMask: mask];
                }
                // The model value shows during a leaf's delay, like the scene's rest frame.
                let rest = if l.band.is_some() { 1f32 } else if l.shimmer { compositor_shimmer(l.color.a, 0.) as f32 } else if l.breathe || l.rotate { 1f32 } else { 0.22f32 };
                let _: () = msg_send![layer, setOpacity: rest];
                let same_curve = existing.is_some_and(|(shown, _)| shown.cycle_ms == l.cycle_ms && shown.delay_ms == l.delay_ms && shown.breathe == l.breathe
                    && shown.shimmer == l.shimmer && shown.rotate == l.rotate && shown.started == l.started && (!l.shimmer || shown.color.a == l.color.a)
                    && shown.band == l.band && shown.band_u == l.band_u && (l.band.is_none() || shown.color == l.color));
                if !same_curve {
                    let animation = Self::compositor_animation(l, layer);
                    let key: *mut objc::runtime::Object = msg_send![objc::class!(NSString), stringWithUTF8String: c"kybern-loop".as_ptr()];
                    let _: () = msg_send![layer, addAnimation: animation forKey: key];
                }
            }
            self.compositor_layers.insert((l.id, l.part), (*l, layer));
        }
        if std::env::var_os("KYBERN_DEBUG_COMPOSITOR").is_some() {
            eprintln!("[compositor-loops] layers={}", self.compositor_layers.len());
        }
    }
}
