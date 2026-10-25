// Included in gpui::window. Native platform and automation dispatch share this
// boundary. One bounded slot, one native delta update per display callback.
struct ScrollInputBatch {
    event: crate::ScrollWheelEvent,
    target: u64,
    first: std::time::Instant,
    last: std::time::Instant,
    packets: usize,
    started: bool,
    terminal: Option<crate::TouchPhase>,
}
#[derive(Default)]
struct ScrollInput {
    target: Option<Rc<dyn Fn(Point<Pixels>) -> Option<u64>>>,
    pending: Option<ScrollInputBatch>,
    scheduled: bool,
    dispatching: bool,
    paint: Option<(std::time::Instant, std::time::Instant, usize)>,
}
impl Window {
    /// Set the painted viewport predicate for native wheel accumulation.
    pub fn set_scroll_input_target(&mut self, target: impl Fn(Point<Pixels>) -> Option<u64> + 'static) {
        self.scroll_input.target = Some(Rc::new(target));
    }
    fn scroll_input_target(&self, position: Point<Pixels>) -> Option<u64> {
        static MODE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        match MODE.get_or_init(|| std::env::var("KYBERN_SCROLL_COALESCE").unwrap_or_else(|_| "pr".into())).as_str() {
            "0" => None,
            // Experimental app-wide wheel mode. Default remains PR-only.
            "all" => Some(0),
            _ => self.scroll_input.target.as_ref().and_then(|target| target(position)),
        }
    }
    fn queue_scroll_input(&mut self, event: &crate::ScrollWheelEvent, cx: &mut App) -> bool {
        if self.scroll_input.dispatching || !event.delta.precise() { return false; }
        let Some(target) = self.scroll_input_target(event.position) else { return false; };
        if self.scroll_input.pending.as_ref().is_some_and(|old| old.target != target || old.event.modifiers != event.modifiers) {
            self.flush_scroll_input(cx);
        }
        let now = std::time::Instant::now();
        if let Some(old) = self.scroll_input.pending.as_mut() {
            // Preserve distance, ignore zero phase markers, and use the newest
            // direction on reversal (the same policy as GPUI List).
            if let (crate::ScrollDelta::Pixels(a), crate::ScrollDelta::Pixels(b)) = (old.event.delta, event.delta) {
                let merge = |a: Pixels, b: Pixels| if b == px(0.) {a} else if a.signum() == b.signum() {a+b} else {b};
                old.event.delta = crate::ScrollDelta::Pixels(point(merge(a.x,b.x), merge(a.y,b.y)));
            }
            old.event.position = event.position;
            old.last = now;
            old.packets += 1;
            // A later contact/momentum packet supersedes an earlier terminal
            // marker in this display batch, while retaining its axis reset.
            if event.touch_phase == crate::TouchPhase::Started || (old.terminal.is_some() && event.touch_phase == crate::TouchPhase::Moved) {
                old.started = true;
                old.terminal = None;
            }
            if matches!(event.touch_phase, crate::TouchPhase::Ended | crate::TouchPhase::Cancelled) {old.terminal=Some(event.touch_phase);}
        } else {
            self.scroll_input.pending = Some(ScrollInputBatch {
                event: event.clone(), target, first: now, last: now, packets: 1,
                started: event.touch_phase == crate::TouchPhase::Started,
                terminal: matches!(event.touch_phase, crate::TouchPhase::Ended | crate::TouchPhase::Cancelled).then_some(event.touch_phase),
            });
        }
        if !self.scroll_input.scheduled {
            self.scroll_input.scheduled = true;
            self.on_next_frame(|window,cx| {
                window.scroll_input.scheduled = false;
                window.flush_scroll_input(cx);
            });
        }
        true
    }
    fn flush_scroll_input(&mut self, cx: &mut App) {
        let Some(batch) = self.scroll_input.pending.take() else {return;};
        // A navigation/unmount between input and display must not scroll the
        // newly mounted page. Bounds are read live from the last painted frame.
        if self.scroll_input_target(batch.event.position) != Some(batch.target) {return;}
        let updates_before=self.invalidator.update_count();
        self.scroll_input.dispatching = true;
        let mut event = batch.event;
        if batch.started {
            let mut phase=event.clone();phase.touch_phase=crate::TouchPhase::Started;
            phase.delta=crate::ScrollDelta::Pixels(point(px(0.),px(0.)));
            self.dispatch_event(PlatformInput::ScrollWheel(phase),cx);
        }
        // Carry the terminal phase on the merged delta. A separate zero end
        // packet would reset List's captured-frame accumulator and undo it.
        event.touch_phase=batch.terminal.unwrap_or(crate::TouchPhase::Moved);
        self.dispatch_event(PlatformInput::ScrollWheel(event),cx);
        self.scroll_input.dispatching = false;
        // A clamped/no-op scroll needs no paint and must not leave an old
        // latency probe attached to an unrelated future frame.
        if self.invalidator.update_count()==updates_before && !self.invalidator.is_dirty() {return;}
        if let Some((first,_,packets))=self.scroll_input.paint.take() {
            self.scroll_input.paint=Some((first,batch.last,packets+batch.packets));
        } else {self.scroll_input.paint=Some((batch.first,batch.last,batch.packets));}
    }
    fn record_direct_scroll_input(&mut self) {
        let now=std::time::Instant::now();
        if let Some((first,_,packets))=self.scroll_input.paint.take() {
            self.scroll_input.paint=Some((first,now,packets+1));
        } else {self.scroll_input.paint=Some((now,now,1));}
    }
    fn scroll_input_painted(&mut self) {
        if let Some((first,last,packets))=self.scroll_input.paint.take() {
            static LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *LOG.get_or_init(||std::env::var_os("KYBERN_FRAME_COSTS").is_some()) {
                eprintln!("[scroll-input-paint] packets={} firstMs={:.3} lastMs={:.3} pending={}",packets,first.elapsed().as_secs_f64()*1000.,last.elapsed().as_secs_f64()*1000.,usize::from(self.scroll_input.pending.is_some()));
            }
        }
    }
}

pub(crate) fn scroll_input_frame_draw_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(||std::env::var("KYBERN_INPUT_FRAME_DRAW").as_deref()==Ok("1"))
}
