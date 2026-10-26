//! System notifications over the XDG notifications D-Bus interface, via
//! `notify-rust`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    ForegroundExecutor, SharedString, SystemNotification, SystemNotificationResponse, Task,
};

/// The XDG action key invoked when the user activates the notification body
/// rather than a specific action button.
const DEFAULT_ACTION: &str = "default";

/// The server-side notification a tag last produced. A tag that is posted
/// again replaces that notification (`replaces_id`), so the thread that waits
/// on the older post must stop reporting: both would hear the same id.
struct Slot {
    server_id: u32,
    generation: u64,
}

type Slots = Arc<Mutex<HashMap<SharedString, Slot>>>;

type ResponseCallback = Rc<RefCell<Option<Box<dyn FnMut(SystemNotificationResponse)>>>>;

pub(crate) struct SystemNotificationState {
    response_sender: mpsc::UnboundedSender<SystemNotificationResponse>,
    response_receiver: Option<mpsc::UnboundedReceiver<SystemNotificationResponse>>,
    callback: ResponseCallback,
    _response_task: Option<Task<()>>,
    slots: Slots,
    generation: Arc<AtomicU64>,
}

impl SystemNotificationState {
    pub(crate) fn new() -> Self {
        let (response_sender, response_receiver) = mpsc::unbounded();
        Self {
            response_sender,
            response_receiver: Some(response_receiver),
            callback: Rc::new(RefCell::new(None)),
            _response_task: None,
            slots: Arc::new(Mutex::new(HashMap::new())),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// `identity` is the `(identifier, name)` given to `set_app_identity`. The
    /// identifier doubles as the desktop entry and the icon name, which is how
    /// an installed app (`<identifier>.desktop`, an icon of the same name)
    /// shows its own icon and groups its notifications.
    pub(crate) fn show(&self, identity: Option<(&str, &str)>, notification: SystemNotification) {
        let mut builder = notify_rust::Notification::new();
        if let Some((identifier, name)) = identity {
            builder
                .appname(name)
                .icon(identifier)
                .hint(notify_rust::Hint::DesktopEntry(identifier.to_string()));
        }
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let replaces = self
            .slots
            .lock()
            .ok()
            .and_then(|slots| slots.get(&notification.tag).map(|slot| slot.server_id));
        if let Some(server_id) = replaces {
            builder.id(server_id);
        }
        builder
            .summary(&notification.title)
            .body(&notification.body)
            .sound_name("message-new-instant")
            .action(DEFAULT_ACTION, DEFAULT_ACTION);
        let mut action_ids = HashMap::new();
        for (index, action) in notification.actions.iter().enumerate() {
            let transport_id = format!("gpui-action-{index}");
            builder.action(&transport_id, &action.label);
            action_ids.insert(transport_id, action.id.clone());
        }
        let built = builder.finalize();

        let sender = self.response_sender.clone();
        let slots = self.slots.clone();
        let tag = notification.tag.clone();
        // `show` connects to the session bus and `wait_for_action` blocks
        // until the notification closes, so both run off the UI thread.
        let spawn_result = std::thread::Builder::new()
            .name("system-notification".to_string())
            .spawn(move || match built.show() {
                Ok(handle) => {
                    if !claim_slot(&slots, &tag, handle.id(), generation) {
                        return;
                    }
                    handle.wait_for_action(|action| {
                        let action_id = match action {
                            // Sent by `notify-rust` when the notification closes
                            // without being activated.
                            "__closed" => {
                                release_slot(&slots, &tag, generation);
                                return;
                            }
                            transport_id => {
                                let Some(action_id) = response_action_id(transport_id, &action_ids)
                                else {
                                    log::warn!(
                                        "system notification returned unknown action {transport_id:?}"
                                    );
                                    return;
                                };
                                action_id
                            }
                        };
                        // A newer post under this tag replaced this one on the
                        // server; its own thread reports the response.
                        if !is_current_slot(&slots, &tag, generation) {
                            return;
                        }
                        sender
                            .unbounded_send(SystemNotificationResponse { tag, action_id })
                            .ok();
                    })
                }
                // No notification daemon (a headless session, a minimal window
                // manager) is an ordinary setup, not a fault.
                Err(error) => log::debug!("failed to show system notification: {error}"),
            });
        if let Err(error) = spawn_result {
            log::warn!("failed to spawn system notification thread: {error}");
        }
    }

    pub(crate) fn dismiss(&self, _tag: &str) {
        // The XDG notifications protocol only allows closing via the
        // server-assigned id on a live handle, which `wait_for_action`
        // consumes; stale notifications simply age out of the shade.
    }

    pub(crate) fn on_response(
        &mut self,
        executor: &ForegroundExecutor,
        callback: Box<dyn FnMut(SystemNotificationResponse)>,
    ) {
        *self.callback.borrow_mut() = Some(callback);

        // Responses arrive from per-notification threads; this task hands
        // them to the registered callback on the main thread.
        if let Some(mut receiver) = self.response_receiver.take() {
            let callback = self.callback.clone();
            self._response_task = Some(executor.spawn(async move {
                while let Some(response) = receiver.next().await {
                    // Take the callback out for the call: it may re-enter the
                    // platform or replace itself.
                    let taken = callback.borrow_mut().take();
                    if let Some(mut taken) = taken {
                        taken(response);
                        callback.borrow_mut().get_or_insert(taken);
                    }
                }
            }));
        }
    }
}

/// Records the server id `generation` of `tag` got. False when a newer post for
/// the tag finished first, which already holds the slot.
fn claim_slot(slots: &Slots, tag: &SharedString, server_id: u32, generation: u64) -> bool {
    let Ok(mut slots) = slots.lock() else {
        return false;
    };
    if slots
        .get(tag)
        .is_some_and(|slot| slot.generation > generation)
    {
        return false;
    }
    slots.insert(
        tag.clone(),
        Slot {
            server_id,
            generation,
        },
    );
    true
}

fn is_current_slot(slots: &Slots, tag: &SharedString, generation: u64) -> bool {
    slots.lock().is_ok_and(|slots| {
        slots
            .get(tag)
            .is_some_and(|slot| slot.generation == generation)
    })
}

/// Forgets the server id when its own notification closed, so the next post
/// under the tag is a new notification rather than a replacement of a gone one.
fn release_slot(slots: &Slots, tag: &SharedString, generation: u64) {
    if let Ok(mut slots) = slots.lock() {
        if slots
            .get(tag)
            .is_some_and(|slot| slot.generation == generation)
        {
            slots.remove(tag);
        }
    }
}

fn response_action_id(
    transport_id: &str,
    action_ids: &HashMap<String, SharedString>,
) -> Option<Option<SharedString>> {
    if transport_id == DEFAULT_ACTION {
        Some(None)
    } else {
        action_ids.get(transport_id).cloned().map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_post_under_the_tag_owns_the_slot() {
        let slots: Slots = Arc::new(Mutex::new(HashMap::new()));
        let tag = SharedString::from("thread-1");
        assert!(claim_slot(&slots, &tag, 7, 1));
        assert!(is_current_slot(&slots, &tag, 1));
        // The replacement keeps the server id and takes over.
        assert!(claim_slot(&slots, &tag, 7, 2));
        assert!(!is_current_slot(&slots, &tag, 1));
        assert!(is_current_slot(&slots, &tag, 2));
        // The older post's thread finishing late neither claims nor releases it.
        assert!(!claim_slot(&slots, &tag, 7, 1));
        release_slot(&slots, &tag, 1);
        assert!(is_current_slot(&slots, &tag, 2));
        release_slot(&slots, &tag, 2);
        assert!(slots.lock().unwrap().is_empty());
    }

    #[test]
    fn caller_action_ids_do_not_collide_with_transport_action_ids() {
        let action_ids = HashMap::from([
            ("gpui-action-0".to_string(), SharedString::from("default")),
            ("gpui-action-1".to_string(), SharedString::from("__closed")),
        ]);

        assert_eq!(response_action_id(DEFAULT_ACTION, &action_ids), Some(None));
        assert_eq!(
            response_action_id("gpui-action-0", &action_ids),
            Some(Some("default".into()))
        );
        assert_eq!(
            response_action_id("gpui-action-1", &action_ids),
            Some(Some("__closed".into()))
        );
        assert_eq!(response_action_id("unknown", &action_ids), None);
    }
}
