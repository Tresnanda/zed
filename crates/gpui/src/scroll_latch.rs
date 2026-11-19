// Kybern: browser-style wheel scroll latching. Included in gpui::window.
//
// Upstream GPUI scrolls every scroller whose hitbox is under the pointer, so a list inside another list (or over one: a
// floating card lets the wheel through to the page behind it) moves together with it. Browsers latch instead:
//
// - The first event of a gesture that has a delta picks one scroller: the frontmost one under the pointer that can still
//   move in that direction. Only it scrolls for the rest of the gesture, including after it reaches its end and through
//   momentum, even when the pointer leaves it.
// - A gesture ends with a new contact (`TouchPhase::Started`) or after `SCROLL_LATCH_GAP` without wheel events (mouse
//   wheels and platforms that only report `Moved`).
// - When nothing under the pointer can move, the next event with a delta picks again.
//
// Only the scrolling is latched: every wheel event still reaches every listener (`on_scroll_wheel`), as DOM wheel events
// do. Scrollers take part through `Window::latch_scroll` (div overflow scrolling, `List`, and custom elements that
// scroll themselves). `KYBERN_SCROLL_LATCH=0` restores the old behaviour for comparisons.
const SCROLL_LATCH_GAP: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ScrollLatchOwner {
    /// No scroller has taken this gesture yet.
    #[default]
    Open,
    Scroller(usize),
}

#[derive(Default)]
struct ScrollLatch {
    owner: ScrollLatchOwner,
    last: Option<web_time::Instant>,
    /// The owner answered the event being dispatched. An owner that no longer paints (closed, unmounted) frees the
    /// gesture after one event instead of swallowing the rest of it.
    owner_seen: bool,
    /// The event being dispatched carries a distance (markers do not, and not every scroller looks at them).
    moving: bool,
    /// The next `Started` is the wheel batching's axis reset for momentum after a lift (`scroll_input.rs`), not a contact.
    continuing: bool,
}

fn scroll_latch_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("KYBERN_SCROLL_LATCH").as_deref() != Ok("0"))
}

impl ScrollLatch {
    fn continue_gesture(&mut self) {
        self.continuing = true;
    }

    fn begin_at(&mut self, phase: crate::TouchPhase, moving: bool, now: web_time::Instant) {
        let quiet = self.last.is_none_or(|last| now.duration_since(last) >= SCROLL_LATCH_GAP);
        let continuing = std::mem::take(&mut self.continuing);
        let contact = phase == crate::TouchPhase::Started && !continuing;
        if contact || quiet {
            self.owner = ScrollLatchOwner::Open;
        }
        self.last = Some(now);
        self.owner_seen = false;
        self.moving = moving;
    }

    fn end(&mut self) {
        if matches!(self.owner, ScrollLatchOwner::Scroller(_)) && self.moving && !self.owner_seen {
            self.owner = ScrollLatchOwner::Open;
        }
    }

    fn decide(&mut self, key: usize, delta: Point<Pixels>, can_scroll: bool) -> bool {
        match self.owner {
            ScrollLatchOwner::Scroller(owner) if owner == key => {
                self.owner_seen = true;
                true
            }
            // Zero-distance phase markers carry no scroll; scrollers still see them (axis locks reset on them).
            _ if delta.x.is_zero() && delta.y.is_zero() => true,
            ScrollLatchOwner::Scroller(_) => false,
            ScrollLatchOwner::Open if can_scroll => {
                self.owner = ScrollLatchOwner::Scroller(key);
                self.owner_seen = true;
                true
            }
            ScrollLatchOwner::Open => false,
        }
    }
}

impl Window {
    fn scroll_latch_begin(&mut self, event: &crate::ScrollWheelEvent) {
        if scroll_latch_enabled() {
            let delta = event.delta.pixel_delta(px(20.));
            let moving = !(delta.x.is_zero() && delta.y.is_zero());
            self.scroll_latch.begin_at(event.touch_phase, moving, web_time::Instant::now());
        }
    }

    fn scroll_latch_end(&mut self) {
        if scroll_latch_enabled() {
            self.scroll_latch.end();
        }
    }

    /// Whether the current wheel gesture is latched to the scroller `key`. Its handler should run even when the pointer
    /// has left it, so a flick keeps going where it started.
    pub fn scroll_latched_to(&self, key: usize) -> bool {
        scroll_latch_enabled() && self.scroll_latch.owner == ScrollLatchOwner::Scroller(key)
    }

    /// Called from a scroller's bubble-phase wheel handler with a stable `key` (the address of its scroll state), the
    /// delta it would apply and whether that delta would move it. `true`: apply the delta (clamped as usual). `false`:
    /// leave the event to the scroller that owns the gesture, or to one behind.
    pub fn latch_scroll(&mut self, key: usize, delta: Point<Pixels>, can_scroll: bool) -> bool {
        !scroll_latch_enabled() || self.scroll_latch.decide(key, delta, can_scroll)
    }
}

/// Whether a scroll offset (GPUI's convention: 0 at the start, `-max` at the end) can move by `delta` on either axis.
pub(crate) fn scroll_offset_can_move(offset: Point<Pixels>, max: Point<Pixels>, delta: Point<Pixels>) -> bool {
    // Half a pixel of slack: rounding leaves offsets a hair short of their ends.
    let slack = px(0.5);
    let axis = |offset: Pixels, max: Pixels, delta: Pixels| {
        (delta > Pixels::ZERO && offset < -slack) || (delta < Pixels::ZERO && offset > -max + slack)
    };
    axis(offset.x, max.x, delta.x) || axis(offset.y, max.y, delta.y)
}

#[cfg(test)]
mod scroll_latch_tests {
    use super::*;
    use crate::{point, TouchPhase};

    fn at(ms: u64, start: web_time::Instant) -> web_time::Instant {
        start + std::time::Duration::from_millis(ms)
    }
    const DOWN: Point<Pixels> = Point { x: Pixels::ZERO, y: px(-10.) };

    #[test]
    fn the_first_scroller_that_can_move_keeps_the_gesture_past_its_end() {
        let (mut latch, t0) = (ScrollLatch::default(), web_time::Instant::now());
        latch.begin_at(TouchPhase::Started, true, t0);
        // Front to back: the inner list can move, the page behind may not.
        assert!(latch.decide(1, DOWN, true));
        assert!(!latch.decide(2, DOWN, true));
        latch.end();
        // Momentum after the list reached its end: still the list's, never the page's.
        latch.begin_at(TouchPhase::Moved, true, at(16, t0));
        assert!(latch.decide(1, DOWN, false));
        assert!(!latch.decide(2, DOWN, true));
        latch.end();
    }

    #[test]
    fn a_new_gesture_at_the_end_goes_to_the_page() {
        let (mut latch, t0) = (ScrollLatch::default(), web_time::Instant::now());
        latch.begin_at(TouchPhase::Started, true, t0);
        assert!(latch.decide(1, DOWN, true));
        latch.end();
        latch.begin_at(TouchPhase::Started, true, at(100, t0));
        assert!(!latch.decide(1, DOWN, false));
        assert!(latch.decide(2, DOWN, true));
        latch.end();
        // Phase-less wheels: a pause starts a new gesture, a steady spin does not.
        latch.begin_at(TouchPhase::Moved, true, at(200, t0));
        assert!(!latch.decide(1, DOWN, true));
        assert!(latch.decide(2, DOWN, true));
        latch.end();
        latch.begin_at(TouchPhase::Moved, true, at(200 + SCROLL_LATCH_GAP.as_millis() as u64, t0));
        assert!(latch.decide(1, DOWN, true));
        latch.end();
    }

    #[test]
    fn momentum_batched_after_a_lift_stays_with_the_owner() {
        let (mut latch, t0) = (ScrollLatch::default(), web_time::Instant::now());
        latch.begin_at(TouchPhase::Started, true, t0);
        assert!(latch.decide(1, DOWN, true));
        latch.end();
        // The batching's synthetic `Started` before momentum that followed the lift in the same frame.
        latch.continue_gesture();
        latch.begin_at(TouchPhase::Started, false, at(16, t0));
        latch.end();
        latch.begin_at(TouchPhase::Moved, true, at(16, t0));
        assert!(latch.decide(1, DOWN, false));
        assert!(!latch.decide(2, DOWN, true));
        latch.end();
        // A real contact afterwards is a new gesture.
        latch.begin_at(TouchPhase::Started, true, at(40, t0));
        assert!(!latch.decide(1, DOWN, false));
        assert!(latch.decide(2, DOWN, true));
    }

    #[test]
    fn markers_pass_and_a_vanished_owner_frees_the_gesture() {
        let (mut latch, t0) = (ScrollLatch::default(), web_time::Instant::now());
        latch.begin_at(TouchPhase::Started, true, t0);
        assert!(latch.decide(2, Point::default(), false), "zero-distance markers reach everyone");
        assert!(latch.decide(1, DOWN, true));
        latch.end();
        // The owner did not answer (it was unmounted): the next event picks again.
        latch.begin_at(TouchPhase::Moved, true, at(16, t0));
        latch.end();
        latch.begin_at(TouchPhase::Moved, true, at(32, t0));
        assert!(latch.decide(2, DOWN, true));
    }

    #[test]
    fn offsets_move_only_inside_their_range() {
        let max = point(px(0.), px(100.));
        assert!(!scroll_offset_can_move(point(px(0.), px(0.)), max, point(px(0.), px(10.))), "at the top, up");
        assert!(scroll_offset_can_move(point(px(0.), px(0.)), max, DOWN));
        assert!(!scroll_offset_can_move(point(px(0.), px(-99.8)), max, DOWN), "at the end, down");
        assert!(scroll_offset_can_move(point(px(0.), px(-99.8)), max, point(px(0.), px(10.))));
        assert!(!scroll_offset_can_move(point(px(0.), px(-50.)), max, point(px(10.), px(0.))), "no horizontal range");
    }
}
