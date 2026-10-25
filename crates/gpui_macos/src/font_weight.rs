// Included in gpui_macos::text_system (font_weight_gpui.py). A variable face with a `wght`
// axis (SF Pro, `.AppleSystemUIFont`) renders the requested CSS weight on that axis, like
// WebKit: 500 is wght 500 rather than font-kit's nearest named face (Regular, because its
// trait mapping puts Medium at 530), and 600/650 are exact instead of Semibold (wght 590).
// The variation font is registered under its own key so CoreText runs map back to it.
mod font_weight_ffi {
    use core_foundation::{base::CFTypeRef, dictionary::CFDictionaryRef};
    use core_graphics::geometry::CGAffineTransform;
    use core_text::{font::CTFontRef, font_descriptor::CTFontDescriptorRef};
    #[link(name = "CoreText", kind = "framework")]
    unsafe extern "C" {
        pub fn CTFontCreateCopyWithAttributes(
            font: CTFontRef,
            size: f64,
            matrix: *const CGAffineTransform,
            attributes: CTFontDescriptorRef,
        ) -> CTFontRef;
        pub fn CTFontCopyVariation(font: CTFontRef) -> CFDictionaryRef;
        pub static kCTFontVariationAxisIdentifierKey: core_foundation::string::CFStringRef;
        pub static kCTFontVariationAxisMinimumValueKey: core_foundation::string::CFStringRef;
        pub static kCTFontVariationAxisMaximumValueKey: core_foundation::string::CFStringRef;
        pub static kCTFontVariationAxisDefaultValueKey: core_foundation::string::CFStringRef;
    }
    pub type _Unused = CFTypeRef;
}

const WGHT_TAG: i64 = 0x7767_6874;

/// The `wght` axis of a face: (minimum, maximum, default). Cached by PostScript name: CoreText localizes every axis
/// name for `CTFontCopyVariationAxes`, far too slow for each shaped run.
fn wght_axis(font: &CTFont) -> Option<(f64, f64, f64)> {
    static CACHE: OnceLock<parking_lot::Mutex<HashMap<String, Option<(f64, f64, f64)>>>> = OnceLock::new();
    let name = font.postscript_name();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(axis) = cache.lock().get(&name) {
        return *axis;
    }
    let axis = wght_axis_uncached(font);
    cache.lock().insert(name, axis);
    axis
}
fn wght_axis_uncached(font: &CTFont) -> Option<(f64, f64, f64)> {
    let axes = font.get_variation_axes()?;
    for axis in axes.iter() {
        let number = |key| unsafe {
            axis.find(CFString::wrap_under_get_rule(key)).and_then(|v| v.downcast::<CFNumber>()).and_then(|n| n.to_f64())
        };
        if number(unsafe { font_weight_ffi::kCTFontVariationAxisIdentifierKey }).map(|tag| tag as i64) != Some(WGHT_TAG) {
            continue;
        }
        return Some((
            number(unsafe { font_weight_ffi::kCTFontVariationAxisMinimumValueKey })?,
            number(unsafe { font_weight_ffi::kCTFontVariationAxisMaximumValueKey })?,
            number(unsafe { font_weight_ffi::kCTFontVariationAxisDefaultValueKey })?,
        ));
    }
    None
}

/// The face's current `wght`, when CoreText reports an explicit variation.
fn wght_value(font: &CTFont) -> Option<f64> {
    unsafe {
        let variation = font_weight_ffi::CTFontCopyVariation(font.as_concrete_TypeRef());
        if variation.is_null() {
            return None;
        }
        let variation: core_foundation::dictionary::CFDictionary<CFNumber, CFNumber> = TCFType::wrap_under_create_rule(variation);
        variation.find(CFNumber::from(WGHT_TAG)).and_then(|v| v.to_f64())
    }
}

/// The PostScript name, plus the `wght` value when the font carries a variation. Variation
/// instances share their face's PostScript name, so the name alone would map a 500 run back
/// to Regular.
fn native_font_key(font: &CTFont) -> String {
    let name = font.postscript_name();
    match wght_value(font) {
        Some(weight) => format!("{name}#wght={}", weight.round() as i64),
        None => name,
    }
}

impl MacTextSystemState {
    /// A run font whose key carries a `wght` that its registered face already has (Regular reports
    /// wght 400 explicitly on some systems) is that face, not a new font.
    fn same_face(&self, font: &CTFont) -> Option<FontId> {
        let weight = wght_value(font)?;
        let id = *self.font_ids_by_postscript_name.get(&font.postscript_name())?;
        static FACE: OnceLock<parking_lot::Mutex<HashMap<usize, Option<f64>>>> = OnceLock::new();
        let faces = FACE.get_or_init(Default::default);
        let cached = faces.lock().get(&id.0).copied();
        let face_weight = match cached {
            Some(weight) => weight,
            None => {
                let face = self.fonts[id.0].native_font();
                let weight = wght_value(&face).or_else(|| wght_axis(&face).map(|axis| axis.2));
                faces.lock().insert(id.0, weight);
                weight
            }
        }?;
        ((face_weight - weight).abs() < 0.5).then_some(id)
    }
    fn weight_variation(&mut self, font_id: FontId, weight: f32) -> FontId {
        static DISABLED: OnceLock<bool> = OnceLock::new();
        if *DISABLED.get_or_init(|| std::env::var("KYBERN_FONT_WEIGHT_AXIS").as_deref() == Ok("0")) || !weight.is_finite() {
            return font_id;
        }
        let base = self.fonts[font_id.0].native_font();
        let Some((min, max, default)) = wght_axis(&base) else {
            return font_id;
        };
        let weight = (weight as f64).clamp(min, max);
        if (wght_value(&base).unwrap_or(default) - weight).abs() < 0.5 {
            return font_id;
        }
        let variation = core_foundation::dictionary::CFDictionary::from_CFType_pairs(&[(CFNumber::from(WGHT_TAG), CFNumber::from(weight))]);
        let attributes = core_foundation::dictionary::CFDictionary::from_CFType_pairs(&[(
            unsafe { CFString::wrap_under_get_rule(core_text::font_descriptor::kCTFontVariationAttribute) },
            variation.as_CFType(),
        )]);
        let descriptor = core_text::font_descriptor::new_from_attributes(&attributes);
        let font: CTFont = unsafe {
            TCFType::wrap_under_create_rule(font_weight_ffi::CTFontCreateCopyWithAttributes(
                base.as_concrete_TypeRef(),
                0.0,
                std::ptr::null(),
                descriptor.as_concrete_TypeRef(),
            ))
        };
        let key = native_font_key(&font);
        if let Some(id) = self.font_ids_by_postscript_name.get(&key) {
            return *id;
        }
        let id = FontId(self.fonts.len());
        self.font_ids_by_postscript_name.insert(key.clone(), id);
        self.postscript_names_by_font_id.insert(id, key);
        self.fonts.push(unsafe { FontKitFont::from_native_font(&font) });
        id
    }
}

thread_local! {
    static SIZED: std::cell::RefCell<HashMap<(usize, u64), CTFont>> = Default::default();
}
/// The run font at `size`, created once per (font, size). CoreText caches named faces itself, but a variation instance
/// copied for every run rebuilds its glyph advances from the variation store each time.
fn sized_native_font(id: FontId, font: &FontKitFont, size: f64) -> CTFont {
    SIZED.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() > 512 {
            cache.clear();
        }
        cache.entry((id.0, size.to_bits())).or_insert_with(|| font.native_font().clone_with_font_size(size)).clone()
    })
}
