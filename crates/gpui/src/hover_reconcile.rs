// Included in gpui::window. Layout and retained-paint movement are hover input,
// but are not mouse movement: never dispatch JS mouseMove or drag/selection work.
pub(crate) struct HoverReconcileEvent {
    pub(crate) retired: bool,
}
impl HoverReconcileEvent {
    pub(crate) fn hovered(&self, hitbox: &Hitbox, window: &Window) -> Option<bool> {
        if self.retired {
            // Rebuilt elements have fresh hitbox ids but the same identity.
            // Replayed cached listeners were moved into the current frame.
            if window
                .rendered_frame
                .hitboxes
                .iter()
                .any(|live| live.id == hitbox.id || hitbox.identity.as_ref().is_some_and(|id| live.identity.as_ref() == Some(id)))
            {
                return None;
            }
            Some(false)
        } else {
            Some(hitbox.is_hovered(window))
        }
    }
}
fn hover_reconcile_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("KYBERN_HOVER_RECONCILE").as_deref() != Ok("0"))
}
impl Window {
    pub(crate) fn on_hover_reconcile(
        &mut self,
        mut listener: impl FnMut(&HoverReconcileEvent, DispatchPhase, &mut Window, &mut App) + 'static,
    ) {
        self.invalidator.debug_assert_paint();
        self.next_frame.mouse_listeners.push(Some(Box::new(move |event, phase, window, cx| {
            if let Some(event) = event.downcast_ref::<HoverReconcileEvent>() {
                listener(event, phase, window, cx);
            }
        })));
    }
    fn notify_hover_listeners(
        &mut self,
        mut listeners: Vec<Option<AnyMouseListener>>,
        event: &HoverReconcileEvent,
        cx: &mut App,
    ) -> Vec<Option<AnyMouseListener>> {
        // Unlike input propagation, every hover owner must observe a leave.
        for listener in listeners.iter_mut().flatten() {
            listener(event, DispatchPhase::Capture, self, cx);
        }
        for listener in listeners.iter_mut().rev().flatten() {
            listener(event, DispatchPhase::Bubble, self, cx);
        }
        listeners
    }
    fn reconcile_layout_hover(&mut self, cx: &mut App) {
        if !hover_reconcile_enabled() {
            return;
        }
        let event = HoverReconcileEvent { retired: true };
        let previous = mem::take(&mut self.next_frame.mouse_listeners);
        self.next_frame.mouse_listeners = self.notify_hover_listeners(previous, &event, cx);
        // draw_roots already tested the completed prepaint hitboxes.
        self.notify_rendered_hover(cx);
    }
    fn reconcile_rendered_hover(&mut self, cx: &mut App) {
        if !hover_reconcile_enabled() {
            return;
        }
        let hit_test = self.rendered_frame.hit_test(self.mouse_position);
        if self.mouse_hit_test != hit_test {
            self.mouse_hit_test = hit_test;
            self.reset_cursor_style(cx);
        }
        self.notify_rendered_hover(cx);
    }
    fn notify_rendered_hover(&mut self, cx: &mut App) {
        let event = HoverReconcileEvent { retired: false };
        let listeners = mem::take(&mut self.rendered_frame.mouse_listeners);
        self.rendered_frame.mouse_listeners = self.notify_hover_listeners(listeners, &event, cx);
    }
}
