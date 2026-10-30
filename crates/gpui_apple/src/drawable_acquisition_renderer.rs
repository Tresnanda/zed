struct AcquiredDrawable {
    drawable: Option<metal::MetalDrawable>,
    waited: std::time::Duration,
}

fn async_drawables_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("KYBERN_ASYNC_DRAWABLE").as_deref() != Ok("0"))
}

impl MetalRenderer {
    fn prefetch_drawable(&mut self, layer: &metal::MetalLayer) {
        let Some(acquisition) = self.drawable_acquisition.as_mut() else { return; };
        let layer = layer.clone();
        let presented = self.presented.clone();
        acquisition.request(move || objc::rc::autoreleasepool(|| {
            let started = std::time::Instant::now();
            wait_for_drawable_faults(&presented);
            let drawable = if drop_drawable_for_test() { None } else { layer.next_drawable().map(ToOwned::to_owned) };
            AcquiredDrawable { drawable, waited: started.elapsed() }
        }));
    }

    // The display callback uses this only when a frame is waiting for a drawable.
    // A prefetched drawable in an otherwise idle window creates no presentation.
    pub fn drawable_is_ready(&self) -> bool {
        self.prepared_drawable.is_some() || self.drawable_acquisition.as_ref().is_some_and(DrawableAcquisition::is_ready)
    }

    fn prepare_drawable(&mut self) -> bool {
        starve_fault(&self.presented);
        let Some(layer) = self.layer.clone() else { return true; };
        if self.prepared_drawable.is_none() {
            if !self.drawable_is_ready() && !self.may_request_drawable() {
                self.dropped_frame = true;
                return false;
            }
            self.prepared_drawable = self.next_drawable(&layer);
        }
        let Some(drawable) = self.prepared_drawable.as_ref() else { return false; };
        let size = layer.drawable_size();
        if drawable.texture().width() != size.width.ceil() as u64 || drawable.texture().height() != size.height.ceil() as u64 {
            // The window resized while acquisition was pending. Never paint a
            // new viewport into an old-sized drawable.
            self.prepared_drawable = None;
            self.prefetch_drawable(&layer);
            self.dropped_frame = true;
            return false;
        }
        true
    }

    fn next_drawable(&mut self, layer: &metal::MetalLayer) -> Option<metal::MetalDrawable> {
        let asynchronous = self.drawable_acquisition.is_some();
        let acquired = if asynchronous {
            self.prefetch_drawable(layer);
            // Usually already prefetched. A new window gets at most 1 ms here;
            // a slow driver keeps working while AppKit and JS process input.
            self.drawable_acquisition.as_mut().unwrap().take(std::time::Duration::from_millis(1))
        } else {
            let started = std::time::Instant::now();
            self.starve_fault_wait();
            let drawable = if drop_drawable_for_test() { None } else { layer.next_drawable().map(ToOwned::to_owned) };
            Some(AcquiredDrawable { drawable, waited: started.elapsed() })
        };
        let Some(AcquiredDrawable { drawable, waited }) = acquired else {
            self.dropped_frame = true;
            return None;
        };
        if asynchronous {
            if drawable.is_some() {
                self.slow_drawables_in_row = 0;
                if let Some(starvation) = self.starvation.take() {
                    let shown = self.presented.shown_count.load(std::sync::atomic::Ordering::Acquire);
                    self.end_starvation(starvation, std::time::Instant::now(), shown, "background drawable acquisition completed");
                }
            } else {
                // A timed-out acquisition still needs the existing layer
                // recovery even when fewer than two presents were tracked.
                self.drawable_waited(waited);
            }
        } else {
            self.drawable_waited(waited);
        }
        if waited.as_millis() > 100 {
            SLOW_DRAWABLES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            MAX_DRAWABLE_WAIT_US.fetch_max(waited.as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
            if std::env::var_os("KYBERN_DRAWABLE_TIMES").is_some() {
                eprintln!("[frame-drawable] {:.3} ms background={asynchronous}", waited.as_secs_f64() * 1000.0);
            }
        }
        if drawable.is_none() {
            DROPPED_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.dropped_frame = true;
        }
        drawable
    }
}
