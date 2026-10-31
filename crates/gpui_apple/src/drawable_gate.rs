// Kybern: drawable starvation (field freeze, 2026-10-29). While WindowServer shows
// none of the drawables this layer presented (seen after the display slept, the
// window still visible to AppKit and its display link running), every
// `nextDrawable` waits about a second and then hands one back anyway
// (`allowsNextDrawableTimeout` is NO, so it never returns nil and no frame counted
// as dropped). The main thread then spent nearly all its time there: no input, no
// JS, nothing new on screen, until a window change made the compositor take the
// layer again.
//
// The gate counts presented drawables until their presented handler runs. With
// `IN_FLIGHT_LIMIT` of them unshown the pool (3) is empty and `nextDrawable` would
// block, so the draw waits for a handler at most until the older one is
// `STARVED_AFTER` old; still nothing, and it skips the draw (the window presents the
// scene again later, `take_dropped_frame`) instead of blocking. A
// `nextDrawable` slower than that twice in a row (or once with the pool full)
// starts the same episode. While starved the layer is kicked: pending CA
// transactions flushed, `presentsWithTransaction` off, `drawableSize` nudged for
// fresh surfaces, at 100 ms and backing off to 3.2 s; a draw is tried again as
// soon as a handler shows the compositor moving, or after the same backoff.
// Presents with a transaction are not counted: their handlers run on the main
// thread, which is the one waiting. `KYBERN_DRAWABLE_GATE=0` turns the gate off;
// `KYBERN_FRAME_STALL_TEST=starve-drawables:<ms>` makes the compositor show
// nothing for 3 s after every <ms> of normal drawing (handlers held back,
// `nextDrawable` blocking 1 s once the pool is empty).

/// Unshown drawables that leave the pool empty (`maximumDrawableCount` 3, one on screen).
const IN_FLIGHT_LIMIT: usize = 2;
/// How long the older of them may stay unshown before the compositor counts as stalled.
const STARVED_AFTER: std::time::Duration = std::time::Duration::from_millis(100);
/// A presented handler this late is taken as lost, so a stale count cannot gate forever.
const LOST_AFTER: std::time::Duration = std::time::Duration::from_secs(4);

static DRAWABLE_STARVATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DRAWABLE_RECOVERIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static STARVED_SKIPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static DRAWABLE_KICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LAST_STARVATION: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Starvation episodes begun and ended, draws skipped, kicks, and the last episode in words.
pub fn starvation_stats() -> ([u64; 4], Option<String>) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        [DRAWABLE_STARVATIONS.load(Relaxed), DRAWABLE_RECOVERIES.load(Relaxed), STARVED_SKIPS.load(Relaxed), DRAWABLE_KICKS.load(Relaxed)],
        LAST_STARVATION.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone(),
    )
}

fn drawable_gate_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("KYBERN_DRAWABLE_GATE").as_deref() != Ok("0"))
}

/// Drawables presented and not yet shown, oldest first, shared with their presented handlers.
#[derive(Default)]
struct PresentedDrawables {
    queue: std::sync::Mutex<std::collections::VecDeque<(u64, std::time::Instant)>>,
    shown: std::sync::Condvar,
    shown_count: std::sync::atomic::AtomicU64,
    /// Handlers held back by the `starve-drawables` fault.
    held: std::sync::Mutex<Vec<u64>>,
}

impl PresentedDrawables {
    fn mark_shown(&self, seq: u64) {
        if STARVE_ACTIVE.load(std::sync::atomic::Ordering::Acquire) {
            self.held.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(seq);
            return;
        }
        let mut queue = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = queue.iter().position(|(entry, _)| *entry == seq) {
            queue.remove(index);
        }
        self.shown_count.fetch_add(1, std::sync::atomic::Ordering::Release);
        self.shown.notify_all();
    }

    /// Unshown drawables now, after waiting up to `wait` past the older one's present
    /// for the count to drop below the limit.
    fn in_flight(&self, wait: std::time::Duration) -> usize {
        let mut queue = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            let now = std::time::Instant::now();
            while queue.front().is_some_and(|(_, at)| now.duration_since(*at) > LOST_AFTER) {
                queue.pop_front();
            }
            if queue.len() < IN_FLIGHT_LIMIT {
                return queue.len();
            }
            let deadline = queue[queue.len() - IN_FLIGHT_LIMIT].1 + wait;
            if now >= deadline {
                return queue.len();
            }
            queue = self.shown.wait_timeout(queue, deadline - now).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
    }
}

struct Starvation {
    since: std::time::Instant,
    cause: &'static str,
    kicks: u32,
    last_kick: Option<std::time::Instant>,
    next_kick: std::time::Instant,
    retry_at: std::time::Instant,
    shown_at: u64,
    skipped: u64,
}

fn starvation_backoff(kicks: u32) -> std::time::Duration {
    std::time::Duration::from_millis(100) * (1 << kicks.saturating_sub(1).min(5))
}

static STARVE_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `KYBERN_FRAME_STALL_TEST=starve-drawables:<ms>`: whether the emulated stalled
/// compositor is on now (3 s after every <ms> off), switching it from the main thread.
fn starve_fault_every() -> Option<std::time::Duration> {
    static EVERY: std::sync::OnceLock<Option<std::time::Duration>> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| {
        let value = std::env::var("KYBERN_FRAME_STALL_TEST").ok()?;
        let ms = value.strip_prefix("starve-drawables:")?;
        Some(std::time::Duration::from_millis(ms.parse().ok()?))
    })
}

fn starve_fault(presented: &PresentedDrawables) -> bool {
    static WINDOW: std::sync::Mutex<Option<(std::time::Instant, bool)>> = std::sync::Mutex::new(None);
    let Some(every) = starve_fault_every() else {
        return false;
    };
    let now = std::time::Instant::now();
    let mut window = WINDOW.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (started, active) = *window.get_or_insert((now, false));
    let elapsed = now.duration_since(started);
    if !active && elapsed >= every {
        *window = Some((now, true));
        STARVE_ACTIVE.store(true, std::sync::atomic::Ordering::Release);
        eprintln!("[frame-drawable] test: the compositor stops showing drawables");
        return true;
    }
    if active && elapsed >= std::time::Duration::from_secs(3) {
        *window = Some((now, false));
        STARVE_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
        let held = std::mem::take(&mut *presented.held.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        eprintln!("[frame-drawable] test: the compositor shows drawables again ({} held back)", held.len());
        for seq in held {
            presented.mark_shown(seq);
        }
        return false;
    }
    active
}

impl MetalRenderer {
    /// Before `nextDrawable`: false when the compositor holds the pool and the draw
    /// should be skipped instead of blocking the main thread.
    fn may_request_drawable(&mut self) -> bool {
        starve_fault(&self.presented);
        if !drawable_gate_enabled() || self.layer.is_none() {
            return true;
        }
        let asynchronous = self.drawable_acquisition.is_some();
        let wait = if asynchronous || self.starvation.is_some() { std::time::Duration::ZERO } else { STARVED_AFTER };
        let full = self.presented.in_flight(wait) >= IN_FLIGHT_LIMIT;
        let blocked = full && (!asynchronous || self.presented.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter().rev().nth(IN_FLIGHT_LIMIT - 1).is_some_and(|(_, at)| at.elapsed() >= STARVED_AFTER));
        let now = std::time::Instant::now();
        let shown = self.presented.shown_count.load(std::sync::atomic::Ordering::Acquire);
        match &self.starvation {
            None if !blocked => return true,
            None => self.start_starvation(now, shown, "presented drawables unshown for 100 ms"),
            Some(starvation) if !blocked && (shown != starvation.shown_at || now >= starvation.retry_at) => return true,
            Some(_) => {}
        }
        let starvation = self.starvation.as_mut().expect("starving");
        starvation.skipped += 1;
        STARVED_SKIPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if now >= starvation.next_kick {
            self.kick_drawables(now);
        }
        false
    }

    /// The emulated stalled compositor's `nextDrawable` wait (the real one is about 1 s).
    fn starve_fault_wait(&self) {
        wait_for_drawable_faults(&self.presented);
    }

    /// After `nextDrawable` returned: a slow one starts or prolongs an episode, a fast
    /// one ends it.
    fn drawable_waited(&mut self, waited: std::time::Duration) {
        if !drawable_gate_enabled() {
            return;
        }
        let now = std::time::Instant::now();
        let shown = self.presented.shown_count.load(std::sync::atomic::Ordering::Acquire);
        if waited <= STARVED_AFTER {
            self.slow_drawables_in_row = 0;
            if let Some(starvation) = self.starvation.take() {
                self.end_starvation(starvation, now, shown, "a drawable came at once");
            }
            return;
        }
        self.slow_drawables_in_row = self.slow_drawables_in_row.saturating_add(1);
        match &mut self.starvation {
            Some(starvation) => {
                starvation.retry_at = now + starvation_backoff(starvation.kicks);
                starvation.shown_at = shown;
            }
            None if self.slow_drawables_in_row >= 2 || self.presented.in_flight(std::time::Duration::ZERO) >= IN_FLIGHT_LIMIT => {
                self.start_starvation(now, shown, "nextDrawable waited over 100 ms");
                self.kick_drawables(now);
            }
            None => {}
        }
    }

    /// Counts a presented drawable until its handler runs (or it is taken as lost).
    fn track_presented(&mut self, drawable: &metal::DrawableRef) {
        if !drawable_gate_enabled() && starve_fault_every().is_none() {
            return;
        }
        self.presented_seq += 1;
        let seq = self.presented_seq;
        self.presented.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push_back((seq, std::time::Instant::now()));
        let presented = self.presented.clone();
        let block = ConcreteBlock::new(move |_: &metal::DrawableRef| presented.mark_shown(seq)).copy();
        drawable.add_presented_handler(&block);
    }

    /// Whether a starvation episode began or ended since the last call (the window
    /// then raises a window-state event so the host logs it).
    pub fn take_starvation_changed(&mut self) -> bool {
        std::mem::take(&mut self.starvation_changed)
    }

    fn start_starvation(&mut self, now: std::time::Instant, shown: u64, cause: &'static str) {
        DRAWABLE_STARVATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pwt = self.layer.as_ref().is_some_and(|layer| unsafe { msg_send![&**layer, presentsWithTransaction] });
        let description = format!("DrawableStarved: {cause} (presentsWithTransaction={pwt}); skipping draws, kicking the layer");
        eprintln!("[frame-drawable] {description}");
        log::warn!("{description}");
        *LAST_STARVATION.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(description);
        self.starvation = Some(Starvation { since: now, cause, kicks: 0, last_kick: None, next_kick: now, retry_at: now, shown_at: shown, skipped: 0 });
        self.starvation_changed = true;
    }

    fn end_starvation(&mut self, starvation: Starvation, now: std::time::Instant, shown: u64, how: &str) {
        DRAWABLE_RECOVERIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let compositor = if shown != starvation.shown_at { "the compositor showed drawables again" } else { how };
        let description = format!(
            "DrawableStarved ({}) for {} ms: {} draws skipped, {} kicks; cleared as {compositor}{}",
            starvation.cause,
            now.duration_since(starvation.since).as_millis(),
            starvation.skipped,
            starvation.kicks,
            starvation.last_kick.map_or_else(String::new, |at| format!(", {} ms after the last kick", now.duration_since(at).as_millis())),
        );
        eprintln!("[frame-drawable] {description}");
        log::warn!("{description}");
        *LAST_STARVATION.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(description);
        self.starvation_changed = true;
    }

    /// Whatever may make the compositor give drawables back: commit pending CA
    /// transactions, stop presenting with them, and nudge `drawableSize` so the
    /// next drawable is a fresh surface.
    fn kick_drawables(&mut self, now: std::time::Instant) {
        DRAWABLE_KICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(starvation) = self.starvation.as_mut() {
            starvation.kicks += 1;
            starvation.last_kick = Some(now);
            starvation.next_kick = now + starvation_backoff(starvation.kicks);
            starvation.retry_at = starvation.next_kick;
        }
        unsafe {
            let _: () = msg_send![objc::class!(CATransaction), flush];
        }
        self.presents_with_transaction = false;
        if let Some(layer) = self.layer.as_ref() {
            let mode: bool = unsafe { msg_send![&**layer, presentsWithTransaction] };
            if mode {
                layer.set_presents_with_transaction(false);
            }
            let size: NSSize = unsafe { msg_send![layer.as_ref(), drawableSize] };
            if size.width >= 1.0 && size.height >= 1.0 {
                let nudged = NSSize { width: size.width, height: size.height + 1.0 };
                unsafe {
                    let _: () = msg_send![layer.as_ref(), setDrawableSize: nudged];
                    let _: () = msg_send![layer.as_ref(), setDrawableSize: size];
                }
            }
        }
    }
}

/// `KYBERN_FRAME_STALL_TEST=kick-reentry:<ms>` (ADE-110): every <ms>, a draw invalidates
/// its view's cursor rects and kicks the layer, as a starved draw does in the field.
/// `flush-in-draw:<ms>` instead flushes the CA transaction right there, inside the
/// draw, whatever the kick does: the AppKit callbacks a flush runs must not wait on
/// the window lock the draw holds.
fn kick_reentry_fault() -> Option<bool> {
    static FAULT: std::sync::OnceLock<Option<(std::time::Duration, bool)>> = std::sync::OnceLock::new();
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let (every, flush) = (*FAULT.get_or_init(|| {
        let value = std::env::var("KYBERN_FRAME_STALL_TEST").ok()?;
        let (ms, flush) = match value.strip_prefix("kick-reentry:") {
            Some(ms) => (ms, false),
            None => (value.strip_prefix("flush-in-draw:")?, true),
        };
        Some((std::time::Duration::from_millis(ms.parse().ok()?), flush))
    }))?;
    let now = std::time::Instant::now();
    let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if now.duration_since(*last.get_or_insert(now)) < every {
        return None;
    }
    *last = Some(now);
    Some(flush)
}

impl MetalRenderer {
    pub(crate) fn kick_reentry_test(&mut self) {
        let Some(flush) = kick_reentry_fault() else { return; };
        let Some(layer) = self.layer.as_ref() else { return; };
        unsafe {
            // AppKit makes a layer-backed view its backing layer's delegate.
            let view: cocoa::base::id = msg_send![layer.as_ref(), delegate];
            let window: cocoa::base::id = if view.is_null() { cocoa::base::nil } else { msg_send![view, window] };
            if window.is_null() {
                eprintln!("[frame-drawable] test: no view window to invalidate cursor rects in");
            } else {
                // AppKit updates cursor rects in a flush only for the key window, as the
                // user's front window was in the field. The background test window says it
                // is key instead of activating the app (its class has no `isKeyWindow`).
                static KEY: std::sync::Once = std::sync::Once::new();
                KEY.call_once(|| {
                    extern "C" fn yes(_: &objc::runtime::Object, _: objc::runtime::Sel) -> objc::runtime::BOOL {
                        objc::runtime::YES
                    }
                    let class = objc::runtime::object_getClass(window as *const objc::runtime::Object) as *mut objc::runtime::Class;
                    let imp: objc::runtime::Imp = std::mem::transmute(yes as extern "C" fn(&objc::runtime::Object, objc::runtime::Sel) -> objc::runtime::BOOL);
                    objc::runtime::class_addMethod(class, sel!(isKeyWindow), imp, c"c@:".as_ptr());
                });
                let _: () = msg_send![window, invalidateCursorRectsForView: view];
            }
        }
        if flush {
            eprintln!("[frame-drawable] test: CA flush inside a draw with invalid cursor rects");
            unsafe {
                let _: () = msg_send![objc::class!(CATransaction), flush];
            }
        } else {
            eprintln!("[frame-drawable] test: kick inside a draw with invalid cursor rects");
            self.kick_drawables(std::time::Instant::now());
        }
        eprintln!("[frame-drawable] test: the draw continues");
    }
}

fn wait_for_drawable_faults(presented: &PresentedDrawables) {
    slow_drawable_fault_wait();
    if STARVE_ACTIVE.load(std::sync::atomic::Ordering::Acquire) && presented.in_flight(std::time::Duration::ZERO) >= IN_FLIGHT_LIMIT {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

// Reproduce a slow acquisition while the gate's presented count is below its
// limit. The field log records these waits with the gate enabled too.
fn slow_drawable_fault_wait() {
    static EVERY: std::sync::OnceLock<Option<std::time::Duration>> = std::sync::OnceLock::new();
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let Some(every) = EVERY.get_or_init(|| {
        let value = std::env::var("KYBERN_FRAME_STALL_TEST").ok()?;
        Some(std::time::Duration::from_millis(value.strip_prefix("slow-drawable:")?.parse().ok()?))
    }) else { return; };
    let now = std::time::Instant::now();
    let wait = {
        let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = *last.get_or_insert(now);
        if now.duration_since(previous) >= *every { *last = Some(now); true } else { false }
    };
    if wait {
        eprintln!("[frame-drawable] test: nextDrawable waits with a non-full presented queue");
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
