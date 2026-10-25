// Included in gpui::window (stream_fade_gpui.py): the paint-side API of stream_fade.rs.
impl Window {
    /// Glyphs painted from now on whose centre lies in one of these words (window logical bounds)
    /// fade in from the word's birth over `fade_ms`. Ends with `end_fade_words`.
    pub fn push_fade_words(&mut self, words: &[(Bounds<Pixels>, std::time::Instant, f32)]) {
        let scale = self.scale_factor();
        let list = self.next_frame.scene.active_fade.get_or_insert_with(Vec::new);
        list.extend(words.iter().map(|(bounds, born, fade_ms)| crate::scene::FadeWord {
            bounds: bounds.scale(scale),
            logical: *bounds,
            born: *born,
            fade_ms: *fade_ms,
        }));
    }
    /// Stop tagging glyphs and blur each word that is still visibly blurred (`blur` at its birth),
    /// with a backdrop pass over what was just painted, which later frames shrink to nothing.
    pub fn end_fade_words(&mut self, blur: Pixels) {
        let Some(words) = self.next_frame.scene.active_fade.take() else { return };
        if blur <= px(0.0) {
            return;
        }
        let scale = self.scale_factor();
        let now = std::time::Instant::now();
        for word in words {
            let tag = crate::scene::FadeTag::new(word.born, word.fade_ms, blur.as_f32() * scale);
            if tag.done(now) {
                continue;
            }
            let (current, opacity) = tag.blur_at(tag.progress(now));
            if opacity <= 0.0 {
                continue;
            }
            let before = self.next_frame.scene.len();
            self.paint_backdrop(word.logical.dilate(px(2.0)), px(current / scale), 1.0, px(0.0), 1.0, 0.0);
            if self.next_frame.scene.len() > before {
                self.next_frame.scene.fade_tags.insert(before, tag);
            }
        }
    }
}
